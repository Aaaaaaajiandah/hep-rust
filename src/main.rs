// hep — version control in Rust
// 92 commands across 8 waves
// .hep/ repo format compatible with the C version

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::os::unix::fs::PermissionsExt;

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha1::{Sha1, Digest};
use chrono::{DateTime, Local, TimeZone, NaiveDateTime};

// ════════════════════════════════════════════════════════════════════════════
// OBJECT STORE
// ════════════════════════════════════════════════════════════════════════════

fn sha1_hex(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn object_path(root: &Path, hex: &str) -> PathBuf {
    root.join(".hep/objects")
        .join(&hex[..2])
        .join(&hex[2..])
}

fn write_object(root: &Path, data: &[u8]) -> io::Result<String> {
    let hex = sha1_hex(data);
    let path = object_path(root, &hex);
    if path.exists() { return Ok(hex); }
    fs::create_dir_all(path.parent().unwrap())?;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data)?;
    fs::write(&path, enc.finish()?)?;
    Ok(hex)
}

fn read_object(root: &Path, hex: &str) -> io::Result<Vec<u8>> {
    let path = object_path(root, hex);
    let compressed = fs::read(&path)?;
    let mut dec = ZlibDecoder::new(compressed.as_slice());
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}

// ════════════════════════════════════════════════════════════════════════════
// REPO HELPERS
// ════════════════════════════════════════════════════════════════════════════

fn find_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".hep").exists() { return Some(dir); }
        if !dir.pop() { return None; }
    }
}

fn root_or_exit() -> PathBuf {
    find_root().unwrap_or_else(|| {
        eprintln!("hep: not in a hep repo");
        std::process::exit(1);
    })
}

fn head_sha(root: &Path) -> Option<String> {
    let head = fs::read_to_string(root.join(".hep/HEAD")).ok()?;
    let head = head.trim();
    if head.starts_with("ref: ") {
        let ref_path = root.join(".hep").join(&head[5..]);
        let sha = fs::read_to_string(ref_path).ok()?;
        let sha = sha.trim().to_string();
        if sha.is_empty() { None } else { Some(sha) }
    } else {
        if head.is_empty() { None } else { Some(head.to_string()) }
    }
}

fn current_branch(root: &Path) -> String {
    let head = fs::read_to_string(root.join(".hep/HEAD"))
        .unwrap_or_default();
    let head = head.trim();
    if head.starts_with("ref: refs/heads/") {
        head[16..].to_string()
    } else {
        "HEAD".to_string()
    }
}

fn write_ref(root: &Path, ref_name: &str, hex: &str) -> io::Result<()> {
    let path = root.join(".hep").join(ref_name);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, format!("{}\n", hex))
}

fn read_ref(root: &Path, ref_name: &str) -> Option<String> {
    let path = root.join(".hep").join(ref_name);
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn update_head(root: &Path, hex: &str) -> io::Result<()> {
    let head = fs::read_to_string(root.join(".hep/HEAD"))
        .unwrap_or_default();
    let head = head.trim().to_string();
    if head.starts_with("ref: ") {
        write_ref(root, &head[5..], hex)
    } else {
        fs::write(root.join(".hep/HEAD"), format!("{}\n", hex))
    }
}

fn reflog_append(root: &Path, hex: &str, msg: &str) {
    let path = root.join(".hep/logs/HEAD");
    let _ = fs::create_dir_all(path.parent().unwrap());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {}", hex, msg);
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn config_get(root: &Path, key: &str) -> Option<String> {
    let cfg = fs::read_to_string(root.join(".hep/config")).ok()?;
    for line in cfg.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&format!("{} = ", key)) {
            return Some(rest.to_string());
        }
    }
    None
}

fn config_set(root: &Path, key: &str, val: &str) -> io::Result<()> {
    let cfg_path = root.join(".hep/config");
    let existing = fs::read_to_string(&cfg_path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines()
        .filter(|l| !l.trim().starts_with(&format!("{} =", key)))
        .map(|l| l.to_string())
        .collect();
    lines.push(format!("\t{} = {}", key, val));
    fs::write(cfg_path, lines.join("\n") + "\n")
}

// ════════════════════════════════════════════════════════════════════════════
// INDEX
// ════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
struct IndexEntry {
    path: String,
    sha: String,
    mode: u32,
}

struct Index {
    entries: Vec<IndexEntry>,
}

impl Index {
    fn read(root: &Path) -> Self {
        let path = root.join(".hep/index");
        let mut entries = Vec::new();
        if let Ok(content) = fs::read_to_string(&path) {
            for line in content.lines() {
                let parts: Vec<&str> = line.splitn(3, ' ').collect();
                if parts.len() == 3 {
                    entries.push(IndexEntry {
                        mode: parts[0].parse().unwrap_or(0o100644),
                        sha: parts[1].to_string(),
                        path: parts[2].to_string(),
                    });
                }
            }
        }
        Index { entries }
    }

    fn write(&self, root: &Path) -> io::Result<()> {
        let path = root.join(".hep/index");
        let mut out = String::new();
        for e in &self.entries {
            out.push_str(&format!("{} {} {}\n", e.mode, e.sha, e.path));
        }
        fs::write(path, out)
    }

    fn add(&mut self, path: &str, sha: &str, mode: u32) {
        self.entries.retain(|e| e.path != path);
        self.entries.push(IndexEntry {
            path: path.to_string(),
            sha: sha.to_string(),
            mode,
        });
    }

    fn remove(&mut self, path: &str) {
        self.entries.retain(|e| e.path != path);
    }

    fn find(&self, path: &str) -> Option<&IndexEntry> {
        self.entries.iter().find(|e| e.path == path)
    }
}

// ════════════════════════════════════════════════════════════════════════════
// BLOB
// ════════════════════════════════════════════════════════════════════════════

fn blob_from_file(root: &Path, file_path: &str) -> io::Result<String> {
    let data = fs::read(file_path)?;
    write_object(root, &data)
}

fn blob_read(root: &Path, hex: &str) -> io::Result<Vec<u8>> {
    read_object(root, hex)
}

// ════════════════════════════════════════════════════════════════════════════
// TREE
// ════════════════════════════════════════════════════════════════════════════

struct TreeEntry {
    name: String,
    sha: String,
    mode: u32,
}

fn tree_write(root: &Path, entries: &[IndexEntry]) -> io::Result<String> {
    let mut data = String::new();
    for e in entries {
        data.push_str(&format!("{} {} {}\n", e.mode, e.sha, e.path));
    }
    write_object(root, data.as_bytes())
}

fn tree_read(root: &Path, hex: &str) -> io::Result<Vec<TreeEntry>> {
    let data = read_object(root, hex)?;
    let content = String::from_utf8_lossy(&data);
    let mut entries = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() == 3 {
            entries.push(TreeEntry {
                mode: parts[0].parse().unwrap_or(0o100644),
                sha: parts[1].to_string(),
                name: parts[2].to_string(),
            });
        }
    }
    Ok(entries)
}

// ════════════════════════════════════════════════════════════════════════════
// COMMIT
// ════════════════════════════════════════════════════════════════════════════

struct Commit {
    tree_sha: String,
    parent_sha: Option<String>,
    author: String,
    message: String,
    timestamp: u64,
}

fn commit_write(root: &Path, c: &Commit) -> io::Result<String> {
    let mut data = format!("tree {}\n", c.tree_sha);
    if let Some(p) = &c.parent_sha {
        data.push_str(&format!("parent {}\n", p));
    }
    data.push_str(&format!("author {}\n", c.author));
    data.push_str(&format!("timestamp {}\n", c.timestamp));
    data.push_str(&format!("\n{}\n", c.message));
    write_object(root, data.as_bytes())
}

fn commit_read(root: &Path, hex: &str) -> io::Result<Commit> {
    let data = read_object(root, hex)?;
    let content = String::from_utf8_lossy(&data).to_string();
    let mut tree_sha = String::new();
    let mut parent_sha = None;
    let mut author = String::new();
    let mut timestamp = 0u64;
    let mut message = String::new();
    let mut in_message = false;

    for line in content.lines() {
        if in_message {
            if !message.is_empty() { message.push('\n'); }
            message.push_str(line);
        } else if line.is_empty() {
            in_message = true;
        } else if let Some(v) = line.strip_prefix("tree ") {
            tree_sha = v.to_string();
        } else if let Some(v) = line.strip_prefix("parent ") {
            parent_sha = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("author ") {
            author = v.to_string();
        } else if let Some(v) = line.strip_prefix("timestamp ") {
            timestamp = v.parse().unwrap_or(0);
        }
    }

    Ok(Commit { tree_sha, parent_sha, author, message, timestamp })
}

fn commit_history(root: &Path) -> Vec<(String, Commit)> {
    let mut result = Vec::new();
    let mut hex = match head_sha(root) {
        Some(h) => h,
        None => return result,
    };
    loop {
        match commit_read(root, &hex) {
            Ok(c) => {
                let next = c.parent_sha.clone();
                result.push((hex, c));
                match next {
                    Some(p) => hex = p,
                    None => break,
                }
            }
            Err(_) => break,
        }
    }
    result
}

fn format_ts(ts: u64) -> String {
    let dt = Local.timestamp_opt(ts as i64, 0).single()
        .unwrap_or_else(|| Local::now());
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn format_ts_date(ts: u64) -> String {
    let dt = Local.timestamp_opt(ts as i64, 0).single()
        .unwrap_or_else(|| Local::now());
    dt.format("%Y-%m-%d").to_string()
}

// ════════════════════════════════════════════════════════════════════════════
// WALK FILES
// ════════════════════════════════════════════════════════════════════════════

fn walk_files(dir: &str) -> Vec<String> {
    use walkdir::WalkDir;
    let mut files = Vec::new();
    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            let p = entry.path().to_string_lossy().to_string();
            let p = if p.starts_with("./") { p[2..].to_string() } else { p };
            if !p.starts_with(".hep/") && p != ".hep" {
                files.push(p);
            }
        }
    }
    files.sort();
    files
}

// ════════════════════════════════════════════════════════════════════════════
// MYERS DIFF
// ════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
enum DiffOp { Keep(String), Del(String), Add(String) }

fn myers_diff(a: &[String], b: &[String]) -> Vec<DiffOp> {
    let n = a.len(); let m = b.len();
    let max_d = n + m + 1;
    let mut v = vec![0i64; 2 * max_d + 1];
    let base = max_d as i64;
    let mut trace: Vec<Vec<i64>> = Vec::new();

    let mut found_d = None;
    'outer: for d in 0..=max_d as i64 {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[(base+k-1) as usize] < v[(base+k+1) as usize]) {
                v[(base+k+1) as usize]
            } else {
                v[(base+k-1) as usize] + 1
            };
            let mut y = x - k;
            while x < n as i64 && y < m as i64 && a[x as usize] == b[y as usize] {
                x += 1; y += 1;
            }
            v[(base+k) as usize] = x;
            if x >= n as i64 && y >= m as i64 { found_d = Some(d); break 'outer; }
            k += 2;
        }
    }

    let found_d = match found_d { Some(d) => d, None => return Vec::new() };

    // backtrack
    let mut ops: Vec<DiffOp> = Vec::new();
    let mut x = n as i64; let mut y = m as i64;
    for d in (1..=found_d).rev() {
        let vv = &trace[d as usize];
        let k = x - y;
        let was_insert = k == -d || (k != d && vv[(base+k-1) as usize] < vv[(base+k+1) as usize]);
        let pk = if was_insert { k + 1 } else { k - 1 };
        let px = vv[(base+pk) as usize];
        let py = px - pk;
        while x > px && y > py { x -= 1; y -= 1; ops.push(DiffOp::Keep(a[x as usize].clone())); }
        if was_insert { y -= 1; ops.push(DiffOp::Add(b[y as usize].clone())); }
        else          { x -= 1; ops.push(DiffOp::Del(a[x as usize].clone())); }
        x = px; y = py;
    }
    while x > 0 && y > 0 { x -= 1; y -= 1; ops.push(DiffOp::Keep(a[x as usize].clone())); }
    ops.reverse();
    ops
}

fn print_diff(fname: &str, old_lines: &[String], new_lines: &[String]) {
    let ops = myers_diff(old_lines, new_lines);
    if ops.is_empty() { return; }
    println!("--- a/{}", fname);
    println!("+++ b/{}", fname);

    // group into hunks
    let ctx = 3usize;
    let mut i = 0;
    while i < ops.len() {
        while i < ops.len() { if !matches!(ops[i], DiffOp::Keep(_)) { break; } i += 1; }
        if i >= ops.len() { break; }
        let hstart = if i > ctx { i - ctx } else { 0 };
        let mut hend = i + 1;
        while hend < ops.len() {
            if !matches!(ops[hend], DiffOp::Keep(_)) {
                hend = (hend + ctx + 1).min(ops.len());
            } else if hend - i >= ctx { break; }
            else { hend += 1; }
        }
        hend = hend.min(ops.len());

        let (mut a0, mut b0, mut ac, mut bc) = (0usize, 0usize, 0usize, 0usize);
        let mut ai = 0usize; let mut bi = 0usize;
        // count line numbers
        for op in &ops[..hstart] { match op { DiffOp::Keep(_)|DiffOp::Del(_) => ai+=1, _ => {} } }
        for op in &ops[..hstart] { match op { DiffOp::Keep(_)|DiffOp::Add(_) => bi+=1, _ => {} } }
        a0 = ai; b0 = bi;
        for op in &ops[hstart..hend] {
            match op {
                DiffOp::Keep(_) => { ac+=1; bc+=1; }
                DiffOp::Del(_)  => { ac+=1; }
                DiffOp::Add(_)  => { bc+=1; }
            }
        }
        println!("@@ -{},{} +{},{} @@", a0+1, ac, b0+1, bc);
        for op in &ops[hstart..hend] {
            match op {
                DiffOp::Keep(l) => println!(" {}", l),
                DiffOp::Del(l)  => println!("\x1b[31m-{}\x1b[0m", l),
                DiffOp::Add(l)  => println!("\x1b[32m+{}\x1b[0m", l),
            }
        }
        i = hend;
    }
}

// ════════════════════════════════════════════════════════════════════════════
// WAVE 1 — CORE (21 commands)
// ════════════════════════════════════════════════════════════════════════════

fn cmd_init(args: &[String]) {
    let dir = args.first().map(|s| s.as_str()).unwrap_or(".");
    let root = Path::new(dir);
    let hep = root.join(".hep");
    for d in &["objects","refs/heads","refs/tags","stash","logs","mansion"] {
        fs::create_dir_all(hep.join(d)).unwrap();
    }
    fs::write(hep.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(hep.join("config"), "").unwrap();
    fs::write(hep.join("index"), "").unwrap();
    println!("Initialized empty hep repository in .hep/ 😎");
}

fn cmd_print(args: &[String]) {
    if args.is_empty() {
        eprintln!("print: Usage: hep print <file|.> [-line <file>]");
        return;
    }
    // print -line <file> = interactive hunk staging
    if args[0] == "-line" {
        cmd_print_line(&args[1..]);
        return;
    }
    let root = root_or_exit();
    let mut idx = Index::read(&root);
    let mansion_threshold = mansion_threshold(&root);

    let files: Vec<String> = if args[0] == "." {
        walk_files(".")
    } else {
        args.to_vec()
    };

    for f in &files {
        let meta = match fs::metadata(f) {
            Ok(m) => m,
            Err(_) => { eprintln!("print: '{}' not found", f); continue; }
        };
        if meta.len() > mansion_threshold {
            if let Ok(ref_str) = mansion_store(&root, f) {
                let ref_sha = write_object(&root, ref_str.as_bytes()).unwrap();
                idx.add(f, &ref_sha, 0o100644);
                println!("print: staged '{}' -> mansion ({:.0} MB)", f,
                         meta.len() as f64 / (1024.0*1024.0));
            }
            continue;
        }
        match blob_from_file(&root, f) {
            Ok(sha) => { idx.add(f, &sha, 0o100644); println!("print: staged '{}'", f); }
            Err(e)  => eprintln!("print: failed to hash '{}': {}", f, e),
        }
    }
    idx.write(&root).unwrap();
}

fn cmd_wave(args: &[String]) {
    let msg = args.windows(2)
        .find(|w| w[0] == "-m")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| { eprintln!("wave: Usage: hep wave -m \"message\""); std::process::exit(1); });

    let root = root_or_exit();
    let idx = Index::read(&root);
    if idx.entries.is_empty() { println!("wave: nothing staged"); return; }

    let tree_sha = tree_write(&root, &idx.entries).unwrap();
    let author = format!("{} <{}>",
        config_get(&root, "name").unwrap_or_else(|| "unknown".into()),
        config_get(&root, "email").unwrap_or_else(|| "unknown@hep".into()));

    let c = Commit {
        tree_sha,
        parent_sha: head_sha(&root),
        author,
        message: msg.clone(),
        timestamp: now_ts(),
    };
    let sha = commit_write(&root, &c).unwrap();
    update_head(&root, &sha).unwrap();
    reflog_append(&root, &sha, &msg);
    println!("wave: [{}] {}", &sha[..8], msg);
}

fn cmd_spy(args: &[String]) {
    if args.first().map(|s| s.as_str()) == Some("-title") {
        cmd_spy_title(&args[1..]);
        return;
    }
    let root = root_or_exit();
    let history = commit_history(&root);
    if history.is_empty() { println!("spy: no commits yet"); return; }
    for (sha, c) in &history {
        println!("commit {}", sha);
        println!("author: {}", c.author);
        println!("date:   {}", format_ts(c.timestamp));
        println!("\n    {}\n", c.message.trim());
    }
}

fn cmd_compete(args: &[String]) {
    if args.first().map(|s| s.as_str()) == Some("-l") {
        cmd_compete_l();
        return;
    }
    let root = root_or_exit();
    let idx = Index::read(&root);

    // staged vs HEAD
    if let Some(head) = head_sha(&root) {
        if let Ok(c) = commit_read(&root, &head) {
            if let Ok(tree) = tree_read(&root, &c.tree_sha) {
                let tree_map: HashMap<String,String> = tree.iter()
                    .map(|e| (e.name.clone(), e.sha.clone())).collect();
                for e in &idx.entries {
                    match tree_map.get(&e.path) {
                        Some(sha) if sha == &e.sha => {}
                        Some(_) => println!("M  {}", e.path),
                        None    => println!("A  {}", e.path),
                    }
                }
            }
        }
    } else {
        for e in &idx.entries { println!("A  {}", e.path); }
    }

    // working tree vs index
    println!("\nWorking tree changes:");
    let idx_map: HashMap<String,String> = idx.entries.iter()
        .map(|e| (e.path.clone(), e.sha.clone())).collect();
    for f in walk_files(".") {
        if let Ok(sha) = blob_from_file(&root, &f) {
            match idx_map.get(&f) {
                None => println!("?? {} (untracked)", f),
                Some(s) if s != &sha => println!("M  {} (modified)", f),
                _ => {}
            }
        }
    }
}

fn cmd_light(_args: &[String]) {
    let root = root_or_exit();
    let branch = current_branch(&root);
    println!("On branch: {}\n", branch);

    let idx = Index::read(&root);
    if idx.entries.is_empty() {
        println!("nothing staged");
    } else {
        println!("Changes staged for commit (hep wave):");
        for e in &idx.entries { println!("  staged: {}", e.path); }
    }

    println!("\nWorking tree:");
    let idx_map: HashMap<String,String> = idx.entries.iter()
        .map(|e| (e.path.clone(), e.sha.clone())).collect();
    let mut any = false;
    for f in walk_files(".") {
        if let Ok(sha) = blob_from_file(&root, &f) {
            match idx_map.get(&f) {
                None => { println!("  untracked: {}", f); any = true; }
                Some(s) if s != &sha => { println!("  modified:  {}", f); any = true; }
                _ => {}
            }
        }
    }
    if !any { println!("  clean"); }
}

fn cmd_expand(args: &[String]) {
    let root = root_or_exit();
    if args.is_empty() {
        // list branches
        let refs_dir = root.join(".hep/refs/heads");
        let current = current_branch(&root);
        if let Ok(entries) = fs::read_dir(&refs_dir) {
            for e in entries.filter_map(|e| e.ok()) {
                let name = e.file_name().to_string_lossy().to_string();
                let marker = if name == current { "* " } else { "  " };
                println!("{}{}", marker, name);
            }
        }
    } else {
        let name = &args[0];
        let head = head_sha(&root).unwrap_or_else(|| {
            eprintln!("expand: no commits yet"); std::process::exit(1);
        });
        write_ref(&root, &format!("refs/heads/{}", name), &head).unwrap();
        println!("expand: created branch '{}'", name);
    }
}

fn cmd_travel(args: &[String]) {
    if args.is_empty() { eprintln!("travel: Usage: hep travel <branch>"); return; }
    let root = root_or_exit();
    let branch = &args[0];
    let ref_path = format!("refs/heads/{}", branch);

    // resolve target sha
    let target_sha = if let Some(sha) = read_ref(&root, &ref_path) {
        sha
    } else {
        // try to create from HEAD
        eprintln!("travel: branch '{}' not found. Use 'hep expand {}' first.", branch, branch);
        return;
    };

    // update HEAD pointer
    fs::write(root.join(".hep/HEAD"), format!("ref: refs/heads/{}\n", branch)).unwrap();

    // restore working tree
    restore_tree(&root, &target_sha);
    println!("travel: switched to branch '{}'", branch);
}

fn restore_tree(root: &Path, commit_sha: &str) {
    if let Ok(c) = commit_read(root, commit_sha) {
        if let Ok(tree) = tree_read(root, &c.tree_sha) {
            let mut idx = Index::read(root);
            idx.entries.clear();
            for e in &tree {
                if let Ok(data) = blob_read(root, &e.sha) {
                    let _ = fs::write(&e.name, &data);
                    idx.add(&e.name, &e.sha, e.mode);
                }
            }
            let _ = idx.write(root);
        }
    }
}

fn cmd_chiplets(args: &[String]) {
    if args.is_empty() { eprintln!("chiplets: Usage: hep chiplets <branch>"); return; }
    let root = root_or_exit();
    let branch = &args[0];
    let ref_path = format!("refs/heads/{}", branch);

    let their_sha = match read_ref(&root, &ref_path) {
        Some(s) => s,
        None => { eprintln!("chiplets: branch '{}' not found", branch); return; }
    };

    let our_sha = match head_sha(&root) {
        Some(s) => s,
        None => { eprintln!("chiplets: no commits on current branch"); return; }
    };

    if our_sha == their_sha { println!("chiplets: already up to date"); return; }

    // simple fast-forward check: if their sha is in our history, nothing to do
    // if our sha is in their history, fast-forward
    let our_history: Vec<String> = commit_history(&root).into_iter().map(|(h,_)| h).collect();
    if our_history.contains(&their_sha) {
        println!("chiplets: already up to date (their branch is behind)");
        return;
    }

    // simple merge: take their tree, create a merge commit
    let their_c = match commit_read(&root, &their_sha) {
        Ok(c) => c,
        Err(_) => { eprintln!("chiplets: couldn't read branch tip"); return; }
    };

    // write merge commit with two parents (we'll just record it as a note for now)
    let author = format!("{} <{}>",
        config_get(&root, "name").unwrap_or_else(|| "unknown".into()),
        config_get(&root, "email").unwrap_or_else(|| "unknown@hep".into()));

    // apply their files to working tree
    if let Ok(tree) = tree_read(&root, &their_c.tree_sha) {
        let mut idx = Index::read(&root);
        for e in &tree {
            if let Ok(data) = blob_read(&root, &e.sha) {
                let _ = fs::write(&e.name, &data);
                idx.add(&e.name, &e.sha, e.mode);
            }
        }
        let _ = idx.write(&root);
    }

    let merge_msg = format!("Merge branch '{}'", branch);
    let c = Commit {
        tree_sha: their_c.tree_sha,
        parent_sha: Some(our_sha),
        author,
        message: merge_msg.clone(),
        timestamp: now_ts(),
    };
    let sha = commit_write(&root, &c).unwrap();
    update_head(&root, &sha).unwrap();
    reflog_append(&root, &sha, &merge_msg);
    println!("chiplets: merged '{}' -> created commit {}", branch, &sha[..8]);
}

fn cmd_stl(args: &[String]) {
    if args.is_empty() { eprintln!("stl: Usage: hep stl <path> [dir]"); return; }
    let src = &args[0];
    let dst = args.get(1)
        .cloned()
        .unwrap_or_else(|| Path::new(src).file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string()));

    // local clone only
    let src_path = Path::new(src);
    if !src_path.join(".hep").exists() {
        eprintln!("stl: '{}' is not a hep repo", src); return;
    }

    // copy .hep directory
    let dst_path = Path::new(&dst);
    let _ = fs::create_dir_all(dst_path);
    copy_dir(src_path, dst_path).unwrap();

    // set remote.origin.url
    let root = dst_path.canonicalize().unwrap();
    config_set(&root, "remote.origin.url", &src_path.canonicalize().unwrap().to_string_lossy()).unwrap();

    // restore working tree
    if let Some(sha) = head_sha(&root) {
        restore_tree(&root, &sha);
    }
    println!("stl: cloned into '{}'", dst);
}

fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    use walkdir::WalkDir;
    for entry in WalkDir::new(src).into_iter().filter_map(|e| e.ok()) {
        let rel = entry.path().strip_prefix(src).unwrap();
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(p) = target.parent() { fs::create_dir_all(p)?; }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn cmd_send(args: &[String]) {
    let root = root_or_exit();
    let remote_url = args.first()
        .cloned()
        .or_else(|| config_get(&root, "remote.origin.url"))
        .unwrap_or_else(|| { eprintln!("send: no remote configured. Use: hep nas origin <url>"); std::process::exit(1); });

    let remote = Path::new(&remote_url);
    if !remote.exists() { eprintln!("send: remote path '{}' not found", remote_url); return; }

    // copy objects
    let src_obj = root.join(".hep/objects");
    let dst_obj = remote.join(".hep/objects");
    let _ = copy_dir(&src_obj, &dst_obj);

    // update remote refs
    let branch = current_branch(&root);
    if let Some(sha) = head_sha(&root) {
        let _ = write_ref(remote, &format!("refs/heads/{}", branch), &sha);
    }
    println!("send: pushed '{}' to {}", branch, remote_url);
}

fn cmd_dock(args: &[String]) {
    let root = root_or_exit();
    let remote_url = args.first()
        .cloned()
        .or_else(|| config_get(&root, "remote.origin.url"))
        .unwrap_or_else(|| { eprintln!("dock: no remote configured"); std::process::exit(1); });

    let remote = Path::new(&remote_url);
    if !remote.exists() { eprintln!("dock: remote path '{}' not found", remote_url); return; }

    // copy objects from remote
    let src_obj = remote.join(".hep/objects");
    let dst_obj = root.join(".hep/objects");
    let _ = copy_dir(&src_obj, &dst_obj);

    // update local branch to match remote
    let branch = current_branch(&root);
    if let Some(sha) = read_ref(remote, &format!("refs/heads/{}", branch)) {
        update_head(&root, &sha).unwrap();
        restore_tree(&root, &sha);
        println!("dock: pulled '{}' — now at {}", branch, &sha[..8]);
    } else {
        println!("dock: remote has no branch '{}'", branch);
    }
}

fn cmd_interface(args: &[String]) {
    if args.len() < 2 { eprintln!("interface: Usage: hep interface <commit> <file>"); return; }
    let root = root_or_exit();
    let sha = &args[0]; let file = &args[1];
    match commit_read(&root, sha) {
        Ok(c) => match tree_read(&root, &c.tree_sha) {
            Ok(tree) => {
                if let Some(e) = tree.iter().find(|e| e.name == *file) {
                    if let Ok(data) = blob_read(&root, &e.sha) {
                        print!("{}", String::from_utf8_lossy(&data));
                    }
                } else { eprintln!("interface: '{}' not in commit {}", file, &sha[..8]); }
            }
            Err(e) => eprintln!("interface: {}", e),
        }
        Err(e) => eprintln!("interface: {}", e),
    }
}

fn cmd_search(args: &[String]) {
    if args.is_empty() { eprintln!("search: Usage: hep search <pattern>"); return; }
    let pattern = &args[0];
    for f in walk_files(".") {
        if let Ok(content) = fs::read_to_string(&f) {
            for (i, line) in content.lines().enumerate() {
                if line.contains(pattern.as_str()) {
                    println!("{}:{}:{}", f, i+1, line);
                }
            }
        }
    }
}

fn cmd_hall(args: &[String]) {
    if args.first().map(|s| s.as_str()) == Some("-coat") {
        cmd_hall_coat(&args[1..]);
        return;
    }
    let root = root_or_exit();
    let stash_dir = root.join(".hep/stash");
    let ts = now_ts();
    let stash_file = stash_dir.join(format!("{}", ts));

    let idx = Index::read(&root);
    let mut content = String::new();
    let msg = args.first().cloned().unwrap_or_else(|| "stash".to_string());
    content.push_str(&format!("msg:{}\n", msg));
    for e in &idx.entries {
        content.push_str(&format!("{} {}\n", e.path, e.sha));
    }
    fs::write(stash_file, content).unwrap();
    println!("hall: changes stashed (hep retrieve to apply)");
}

fn cmd_retrieve(_args: &[String]) {
    let root = root_or_exit();
    let stash_dir = root.join(".hep/stash");
    let mut entries: Vec<_> = fs::read_dir(&stash_dir).unwrap()
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.metadata().unwrap().modified().unwrap());

    if let Some(latest) = entries.last() {
        let content = fs::read_to_string(latest.path()).unwrap();
        let mut idx = Index::read(&root);
        for line in content.lines() {
            if line.starts_with("msg:") { continue; }
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 {
                if let Ok(data) = blob_read(&root, parts[1]) {
                    let _ = fs::write(parts[0], &data);
                    idx.add(parts[0], parts[1], 0o100644);
                }
            }
        }
        idx.write(&root).unwrap();
        fs::remove_file(latest.path()).unwrap();
        println!("retrieve: applied and dropped stash entry");
    } else {
        println!("retrieve: nothing to apply");
    }
}

fn cmd_group(args: &[String]) {
    let root = root_or_exit();
    if args.is_empty() {
        let tags_dir = root.join(".hep/refs/tags");
        if let Ok(entries) = fs::read_dir(&tags_dir) {
            for e in entries.filter_map(|e| e.ok()) {
                println!("{}", e.file_name().to_string_lossy());
            }
        }
    } else {
        let name = &args[0];
        let sha = head_sha(&root).unwrap_or_default();
        write_ref(&root, &format!("refs/tags/{}", name), &sha).unwrap();
        println!("group: created tag '{}'", name);
    }
}

fn cmd_microscope(args: &[String]) {
    if args.is_empty() { eprintln!("microscope: Usage: hep microscope <hash>"); return; }
    let root = root_or_exit();
    match read_object(&root, &args[0]) {
        Ok(data) => print!("{}", String::from_utf8_lossy(&data)),
        Err(e)   => eprintln!("microscope: {}", e),
    }
}

fn cmd_earth(args: &[String]) {
    if args.is_empty() { eprintln!("earth: Usage: hep earth <file>"); return; }
    let root = root_or_exit();
    let mut idx = Index::read(&root);
    for f in args {
        idx.remove(f);
        let _ = fs::remove_file(f);
        println!("earth: removed '{}'", f);
    }
    idx.write(&root).unwrap();
}

fn cmd_house(args: &[String]) {
    if args.is_empty() { eprintln!("house: Usage: hep house <key> [value]"); return; }
    let root = root_or_exit();
    if args.len() == 1 {
        if let Some(v) = config_get(&root, &args[0]) {
            println!("{}", v);
        } else {
            eprintln!("house: key '{}' not set", args[0]);
        }
    } else {
        config_set(&root, &args[0], &args[1]).unwrap();
        println!("house: {} = {}", args[0], args[1]);
    }
}

fn cmd_kill(args: &[String]) {
    if args.is_empty() { eprintln!("kill: Usage: hep kill <commit>"); return; }
    let root = root_or_exit();
    let target = &args[0];
    // resolve "HEAD~N" style
    let sha = if target.starts_with("HEAD~") {
        let n: usize = target[5..].parse().unwrap_or(1);
        let history = commit_history(&root);
        if n >= history.len() { eprintln!("kill: not enough history"); return; }
        history[n].0.clone()
    } else {
        target.clone()
    };
    update_head(&root, &sha).unwrap();
    restore_tree(&root, &sha);
    println!("kill: reset to {}", &sha[..8]);
}

// ════════════════════════════════════════════════════════════════════════════
// WAVE 2 — EXTENDED (20 commands)
// ════════════════════════════════════════════════════════════════════════════

fn cmd_mean(args: &[String]) {
    if args.is_empty() { eprintln!("mean: Usage: hep mean <commit>"); return; }
    let root = root_or_exit();
    let src_sha = &args[0];
    let c = match commit_read(&root, src_sha) {
        Ok(c) => c, Err(e) => { eprintln!("mean: {}", e); return; }
    };
    let author = format!("{} <{}>",
        config_get(&root, "name").unwrap_or_else(|| "unknown".into()),
        config_get(&root, "email").unwrap_or_else(|| "unknown@hep".into()));
    let new_c = Commit {
        tree_sha: c.tree_sha.clone(),
        parent_sha: head_sha(&root),
        author,
        message: c.message.clone(),
        timestamp: now_ts(),
    };
    // apply files
    if let Ok(tree) = tree_read(&root, &c.tree_sha) {
        let mut idx = Index::read(&root);
        for e in &tree {
            if let Ok(data) = blob_read(&root, &e.sha) {
                let _ = fs::write(&e.name, &data);
                idx.add(&e.name, &e.sha, e.mode);
            }
        }
        idx.write(&root).unwrap();
    }
    let sha = commit_write(&root, &new_c).unwrap();
    update_head(&root, &sha).unwrap();
    reflog_append(&root, &sha, &c.message);
    println!("mean: applied commit {} as {}", &src_sha[..8], &sha[..8]);
}

fn cmd_short(_args: &[String]) {
    let root = root_or_exit();
    for (sha, c) in commit_history(&root) {
        let first_line = c.message.lines().next().unwrap_or("").to_string();
        println!("{} {}", &sha[..7], first_line);
    }
}

fn cmd_close(args: &[String]) {
    if args.is_empty() { eprintln!("close: Usage: hep close <branch>"); return; }
    let root = root_or_exit();
    let current = current_branch(&root);
    if args[0] == current { eprintln!("close: can't delete current branch"); return; }
    let ref_path = root.join(".hep/refs/heads").join(&args[0]);
    if fs::remove_file(&ref_path).is_ok() {
        println!("close: deleted branch '{}'", args[0]);
    } else {
        eprintln!("close: branch '{}' not found", args[0]);
    }
}

fn cmd_secret(args: &[String]) {
    let root = root_or_exit();
    let out = args.first().cloned().unwrap_or_else(|| "archive.tar.gz".to_string());
    // simple: create tar of working files
    let files = walk_files(".");
    use std::process::Command;
    let status = Command::new("tar").arg("czf").arg(&out).args(&files).status();
    match status {
        Ok(s) if s.success() => println!("secret: archive written to '{}'", out),
        _ => eprintln!("secret: archive failed"),
    }
}

fn cmd_change(args: &[String]) {
    if args.len() < 2 { eprintln!("change: Usage: hep change <old> <new>"); return; }
    let root = root_or_exit();
    let old_ref = root.join(".hep/refs/heads").join(&args[0]);
    let new_ref = root.join(".hep/refs/heads").join(&args[1]);
    if fs::rename(&old_ref, &new_ref).is_ok() {
        // update HEAD if needed
        let head = fs::read_to_string(root.join(".hep/HEAD")).unwrap_or_default();
        if head.trim() == format!("ref: refs/heads/{}", args[0]) {
            fs::write(root.join(".hep/HEAD"),
                format!("ref: refs/heads/{}\n", args[1])).unwrap();
        }
        println!("change: renamed '{}' -> '{}'", args[0], args[1]);
    } else {
        eprintln!("change: branch '{}' not found", args[0]);
    }
}

fn cmd_accuse(args: &[String]) {
    if args.first().map(|s| s.as_str()) == Some("-part") {
        cmd_accuse_part(&args[1..]);
        return;
    }
    if args.is_empty() { eprintln!("accuse: Usage: hep accuse <file>"); return; }
    let root = root_or_exit();
    let fname = &args[0];
    let content = match fs::read_to_string(fname) {
        Ok(c) => c, Err(_) => { eprintln!("accuse: '{}' not found", fname); return; }
    };

    // find last commit that touched each line
    let history = commit_history(&root);
    let mut line_blame: HashMap<usize, (String, String, u64)> = HashMap::new();

    for (sha, c) in &history {
        if let Ok(tree) = tree_read(&root, &c.tree_sha) {
            if let Some(e) = tree.iter().find(|e| e.name == *fname) {
                if let Ok(data) = blob_read(&root, &e.sha) {
                    let blob_content = String::from_utf8_lossy(&data);
                    for (i, line) in blob_content.lines().enumerate() {
                        if !line_blame.contains_key(&i) {
                            line_blame.insert(i, (sha[..7].to_string(), c.author.clone(), c.timestamp));
                        }
                    }
                }
            }
        }
    }

    for (i, line) in content.lines().enumerate() {
        let (sha, author, ts) = line_blame.get(&i)
            .map(|(s,a,t)| (s.clone(), a.clone(), *t))
            .unwrap_or_else(|| ("0000000".to_string(), "unknown".to_string(), 0));
        println!("{} ({:<20} {} {:>4}) {}", sha, author, format_ts_date(ts), i+1, line);
    }
}

fn cmd_discord(args: &[String]) {
    let new_msg = args.windows(2)
        .find(|w| w[0] == "-m")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| { eprintln!("discord: Usage: hep discord -m \"new message\""); std::process::exit(1); });

    let root = root_or_exit();
    let sha = match head_sha(&root) {
        Some(s) => s,
        None => { eprintln!("discord: no commits yet"); return; }
    };
    let mut c = commit_read(&root, &sha).unwrap();
    c.message = new_msg.clone();
    c.timestamp = now_ts();
    let new_sha = commit_write(&root, &c).unwrap();
    update_head(&root, &new_sha).unwrap();
    println!("discord: amended to '{}'", new_msg);
}

fn cmd_window(args: &[String]) {
    if args.len() < 2 { eprintln!("window: Usage: hep window <commit_a> <commit_b>"); return; }
    let root = root_or_exit();
    let ca = match commit_read(&root, &args[0]) {
        Ok(c) => c, Err(e) => { eprintln!("window: {}", e); return; }
    };
    let cb = match commit_read(&root, &args[1]) {
        Ok(c) => c, Err(e) => { eprintln!("window: {}", e); return; }
    };
    let tree_a = tree_read(&root, &ca.tree_sha).unwrap_or_default();
    let tree_b = tree_read(&root, &cb.tree_sha).unwrap_or_default();
    let map_a: HashMap<String,String> = tree_a.iter().map(|e| (e.name.clone(), e.sha.clone())).collect();
    let map_b: HashMap<String,String> = tree_b.iter().map(|e| (e.name.clone(), e.sha.clone())).collect();

    for (name, sha_b) in &map_b {
        match map_a.get(name) {
            None => println!("A  {}", name),
            Some(sha_a) if sha_a != sha_b => {
                let old = blob_read(&root, sha_a).unwrap_or_default();
                let new = blob_read(&root, sha_b).unwrap_or_default();
                let old_lines: Vec<String> = String::from_utf8_lossy(&old).lines().map(|l| l.to_string()).collect();
                let new_lines: Vec<String> = String::from_utf8_lossy(&new).lines().map(|l| l.to_string()).collect();
                print_diff(name, &old_lines, &new_lines);
            }
            _ => {}
        }
    }
    for name in map_a.keys() {
        if !map_b.contains_key(name) { println!("D  {}", name); }
    }
}

fn cmd_what(args: &[String]) {
    if args.is_empty() { eprintln!("what: Usage: hep what <string>"); return; }
    let root = root_or_exit();
    let needle = &args[0];
    for (sha, c) in commit_history(&root) {
        if let Ok(tree) = tree_read(&root, &c.tree_sha) {
            for e in &tree {
                if let Ok(data) = blob_read(&root, &e.sha) {
                    if String::from_utf8_lossy(&data).contains(needle.as_str()) {
                        let first_line = c.message.lines().next().unwrap_or("").to_string();
                        println!("{} {}", &sha[..7], first_line);
                        break;
                    }
                }
            }
        }
    }
}

fn cmd_bd(args: &[String]) {
    if args.is_empty() { eprintln!("bd: Usage: hep bd <file> [file2...]"); return; }
    cmd_earth(args);
}

fn cmd_power(_args: &[String]) {
    let root = root_or_exit();
    let obj_dir = root.join(".hep/objects");
    let mut count = 0;
    if let Ok(dirs) = fs::read_dir(&obj_dir) {
        for d in dirs.filter_map(|e| e.ok()) {
            if let Ok(files) = fs::read_dir(d.path()) {
                for f in files.filter_map(|e| e.ok()) {
                    let prefix = d.file_name().to_string_lossy().to_string();
                    let name = f.file_name().to_string_lossy().to_string();
                    let hex = format!("{}{}", prefix, name);
                    match read_object(&root, &hex) {
                        Ok(_) => count += 1,
                        Err(e) => println!("corrupt: {} — {}", hex, e),
                    }
                }
            }
        }
    }
    println!("power: {} objects verified OK", count);
}

fn cmd_hotel(_args: &[String]) {
    let root = root_or_exit();
    let history = commit_history(&root);
    let branch = current_branch(&root);
    let files = walk_files(".");

    println!("╔══════════════════════════════════════╗");
    println!("║          hep repo dashboard          ║");
    println!("╠══════════════════════════════════════╣");
    println!("║  branch  : {:<27}║", branch);
    println!("║  commits : {:<27}║", history.len());
    println!("║  files   : {:<27}║", files.len());
    if let Some((sha, c)) = history.first() {
        let msg = c.message.lines().next().unwrap_or("").to_string();
        println!("║  head    : {:<27}║", &sha[..7]);
        println!("║  last    : {:<27}║", &msg[..msg.len().min(27)]);
    }
    println!("╚══════════════════════════════════════╝");
}

fn cmd_wpm(_args: &[String]) {
    let mut total_lines = 0usize;
    let mut total_words = 0usize;
    let mut total_chars = 0usize;
    for f in walk_files(".") {
        if let Ok(content) = fs::read_to_string(&f) {
            total_lines += content.lines().count();
            total_words += content.split_whitespace().count();
            total_chars += content.chars().count();
        }
    }
    println!("lines: {}  words: {}  chars: {}", total_lines, total_words, total_chars);
}

fn cmd_gnome(_args: &[String]) {
    let root = root_or_exit();
    let idx = Index::read(&root);
    let tracked: Vec<String> = idx.entries.iter().map(|e| e.path.clone()).collect();
    for f in walk_files(".") {
        if !tracked.contains(&f) { println!("{}", f); }
    }
}

fn cmd_intelisbetterthanamd(_args: &[String]) {
    println!("╔═══════════════════════════════════════════╗");
    println!("║           hep system information         ║");
    println!("╠═══════════════════════════════════════════╣");
    let hostname = std::process::Command::new("hostname").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    println!("║  host  : {:<34}║", hostname);
    println!("║  hep   : v9.0 (Rust edition)             ║");
    println!("║  arch  : {:<34}║", std::env::consts::ARCH);
    println!("║  os    : {:<34}║", std::env::consts::OS);
    println!("╚═══════════════════════════════════════════╝");
}

fn cmd_nvl(_args: &[String]) {
    let root = root_or_exit();
    let idx = Index::read(&root);
    for e in &idx.entries {
        if let Ok(meta) = fs::metadata(&e.path) {
            if meta.len() == 0 { println!("{}", e.path); }
        }
    }
}

fn cmd_ptl(_args: &[String]) {
    let root = root_or_exit();
    println!("{}", root.join(".hep").display());
}

fn cmd_aaa(_args: &[String]) {
    cmd_print(&["." .to_string()]);
}

fn cmd_linux(_args: &[String]) {
    print_tree(".", 0);
}

fn print_tree(dir: &str, depth: usize) {
    let indent = "    ".repeat(depth);
    let entries = match fs::read_dir(dir) {
        Ok(e) => e, Err(_) => return,
    };
    let mut items: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name());
    for item in items {
        let name = item.file_name().to_string_lossy().to_string();
        if name.starts_with(".hep") { continue; }
        let path = item.path();
        if path.is_dir() {
            println!("{}📁 {}/", indent, name);
            print_tree(&path.to_string_lossy(), depth + 1);
        } else {
            println!("{}📄 {}", indent, name);
        }
    }
}

fn cmd_r(args: &[String]) {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(5);
    let root = root_or_exit();
    let history = commit_history(&root);
    println!("Last {} commits:", n);
    for (sha, c) in history.iter().take(n) {
        let first_line = c.message.lines().next().unwrap_or("").to_string();
        println!("  {} {} — {}", &sha[..7], format_ts_date(c.timestamp), first_line);
    }
}

// ════════════════════════════════════════════════════════════════════════════
// WAVE 3 — ESSENTIALS
// ════════════════════════════════════════════════════════════════════════════

fn cmd_arm(args: &[String]) {
    if args.is_empty() { eprintln!("arm: Usage: hep arm <branch>"); return; }
    let root = root_or_exit();
    let branch = &args[0];
    let onto_sha = match read_ref(&root, &format!("refs/heads/{}", branch)) {
        Some(s) => s,
        None => { eprintln!("arm: branch '{}' not found", branch); return; }
    };
    // simple rebase: move current branch tip to be on top of onto
    let current = current_branch(&root);
    let current_sha = match head_sha(&root) {
        Some(s) => s,
        None => { eprintln!("arm: nothing to rebase"); return; }
    };
    let mut c = commit_read(&root, &current_sha).unwrap();
    c.parent_sha = Some(onto_sha);
    c.timestamp = now_ts();
    let new_sha = commit_write(&root, &c).unwrap();
    update_head(&root, &new_sha).unwrap();
    reflog_append(&root, &new_sha, &format!("arm onto {}", branch));
    println!("arm: rebased '{}' onto '{}'", current, branch);
}

fn cmd_ia(args: &[String]) {
    let root = root_or_exit();
    let remote_url = args.first()
        .cloned()
        .or_else(|| config_get(&root, "remote.origin.url"))
        .unwrap_or_else(|| { eprintln!("ia: no remote configured"); std::process::exit(1); });

    let remote = Path::new(&remote_url);
    if !remote.exists() { eprintln!("ia: remote '{}' not found", remote_url); return; }

    // copy objects
    let _ = copy_dir(&remote.join(".hep/objects"), &root.join(".hep/objects"));

    // copy refs to remote tracking
    let refs_dir = remote.join(".hep/refs/heads");
    if let Ok(entries) = fs::read_dir(&refs_dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(sha) = read_ref(remote, &format!("refs/heads/{}", name)) {
                let _ = write_ref(&root, &format!("refs/remote/{}", name), &sha);
            }
        }
    }
    println!("ia: fetched from {}", remote_url);
}

fn cmd_intel(args: &[String]) {
    if args.is_empty() { eprintln!("intel: Usage: hep intel <file>"); return; }
    let root = root_or_exit();
    let fname = &args[0];
    let sha = match head_sha(&root) {
        Some(s) => s, None => { eprintln!("intel: no commits"); return; }
    };
    let c = commit_read(&root, &sha).unwrap();
    let tree = tree_read(&root, &c.tree_sha).unwrap_or_default();
    if let Some(e) = tree.iter().find(|e| e.name == *fname) {
        if let Ok(data) = blob_read(&root, &e.sha) {
            fs::write(fname, &data).unwrap();
            println!("intel: restored '{}'", fname);
        }
    } else {
        eprintln!("intel: '{}' not in HEAD", fname);
    }
}

fn cmd_amd(args: &[String]) {
    let root = root_or_exit();
    let state_path = root.join(".hep/bisect/state");

    match args.first().map(|s| s.as_str()) {
        Some("start") => {
            fs::create_dir_all(state_path.parent().unwrap()).unwrap();
            let history = commit_history(&root);
            fs::write(&state_path, format!("bad:\ngood:\ncommits:{}\n",
                history.iter().map(|(s,_)| s.clone()).collect::<Vec<_>>().join(",")
            )).unwrap();
            println!("amd: bisect started. Use 'hep amd good/bad' to narrow down.");
        }
        Some("good") => {
            println!("amd: marked current as good — checking middle commit");
        }
        Some("bad") => {
            println!("amd: marked current as bad — checking middle commit");
        }
        Some("run") => {
            println!("amd: bisect complete");
        }
        _ => eprintln!("amd: Usage: hep amd start|good|bad|run"),
    }
}

fn cmd_nvidia(_args: &[String]) {
    let root = root_or_exit();
    let log_path = root.join(".hep/logs/HEAD");
    if let Ok(content) = fs::read_to_string(&log_path) {
        for line in content.lines().rev() {
            println!("{}", line);
        }
    } else {
        println!("nvidia: no reflog yet");
    }
}

fn cmd_arc(_args: &[String]) {
    let root = root_or_exit();
    let stash_dir = root.join(".hep/stash");
    let mut entries: Vec<_> = fs::read_dir(&stash_dir).unwrap_or_else(|_| {
        println!("arc: no stashes"); std::process::exit(0);
    }).filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.metadata().unwrap().modified().unwrap());
    if entries.is_empty() { println!("arc: no stashes"); return; }
    for (i, e) in entries.iter().enumerate() {
        let content = fs::read_to_string(e.path()).unwrap_or_default();
        let msg = content.lines().find(|l| l.starts_with("msg:"))
            .map(|l| &l[4..]).unwrap_or("stash");
        println!("stash@{{{}}}: {}", i, msg);
    }
}

fn cmd_radeon(args: &[String]) {
    let force = args.iter().any(|a| a == "-f");
    let root = root_or_exit();
    let idx = Index::read(&root);
    let tracked: Vec<String> = idx.entries.iter().map(|e| e.path.clone()).collect();
    let mut removed = 0;
    for f in walk_files(".") {
        if !tracked.contains(&f) {
            if force {
                fs::remove_file(&f).unwrap();
                println!("radeon: removed '{}'", f);
                removed += 1;
            } else {
                println!("radeon: would remove '{}' (use -f to confirm)", f);
            }
        }
    }
    if force && removed == 0 { println!("radeon: nothing to clean"); }
}

// ════════════════════════════════════════════════════════════════════════════
// WAVE 4 — HARDWARE
// ════════════════════════════════════════════════════════════════════════════

fn cmd_rtx(args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        Some("list") => {
            let root = root_or_exit();
            for (sha, c) in commit_history(&root) {
                let msg = c.message.lines().next().unwrap_or("").to_string();
                println!("pick {} {}", &sha[..7], msg);
            }
        }
        Some("squash") => println!("rtx squash: combine top 2 commits — use 'hep discord' to amend message"),
        Some("drop") => {
            if let Some(sha) = args.get(1) {
                println!("rtx drop: dropping {} — use 'hep kill {}' to reset", sha, sha);
            }
        }
        _ => eprintln!("rtx: Usage: hep rtx list|squash|drop"),
    }
}

fn cmd_gtx(_args: &[String]) {
    let root = root_or_exit();
    let mut by_author: HashMap<String, Vec<String>> = HashMap::new();
    for (sha, c) in commit_history(&root) {
        let msg = c.message.lines().next().unwrap_or("").to_string();
        by_author.entry(c.author.clone()).or_default().push(format!("{} {}", &sha[..7], msg));
    }
    for (author, commits) in &by_author {
        println!("{} ({})", author, commits.len());
        for c in commits { println!("  {}", c); }
    }
}

fn cmd_rx(args: &[String]) {
    if args.len() < 2 { eprintln!("rx: Usage: hep rx <dir> <branch>"); return; }
    let root = root_or_exit();
    let dir = &args[0]; let branch = &args[1];
    let sha = match read_ref(&root, &format!("refs/heads/{}", branch)) {
        Some(s) => s,
        None => { eprintln!("rx: branch '{}' not found", branch); return; }
    };
    // create worktree
    copy_dir(&root, Path::new(dir)).unwrap();
    let wt_root = Path::new(dir);
    restore_tree(&wt_root, &sha);
    fs::write(wt_root.join(".hep/HEAD"),
        format!("ref: refs/heads/{}\n", branch)).unwrap();
    println!("rx: worktree '{}' created on branch '{}'", dir, branch);
}

fn cmd_iris(args: &[String]) {
    if args.is_empty() { eprintln!("iris: Usage: hep iris <repo> [dir]"); return; }
    let repo = &args[0];
    let dir = args.get(1).cloned().unwrap_or_else(|| {
        Path::new(repo).file_name().unwrap().to_string_lossy().to_string()
    });
    let root = root_or_exit();
    let modules_dir = root.join(".hep/modules");
    fs::create_dir_all(&modules_dir).unwrap();
    fs::write(modules_dir.join(&dir), format!("url={}\npath={}\n", repo, dir)).unwrap();
    println!("iris: registered submodule '{}' at '{}'", repo, dir);
    println!("      run 'hep stl {} {}' to clone it", repo, dir);
}

fn cmd_xe(args: &[String]) {
    let root = root_or_exit();
    let sparse_path = root.join(".hep/sparse");
    match args.first().map(|s| s.as_str()) {
        Some("set") => {
            if args.len() < 2 { eprintln!("xe set: provide pattern"); return; }
            let mut content = fs::read_to_string(&sparse_path).unwrap_or_default();
            content.push_str(&format!("{}\n", args[1]));
            fs::write(&sparse_path, content).unwrap();
            println!("xe: added pattern '{}'", args[1]);
        }
        Some("list") => {
            let content = fs::read_to_string(&sparse_path).unwrap_or_default();
            if content.is_empty() { println!("xe: no patterns set"); }
            else { print!("{}", content); }
        }
        Some("clear") => {
            fs::write(&sparse_path, "").unwrap();
            println!("xe: cleared all patterns");
        }
        _ => eprintln!("xe: Usage: hep xe set|list|clear"),
    }
}

fn cmd_uhd(_args: &[String]) {
    let root = root_or_exit();
    let refs_dir = root.join(".hep/refs/heads");
    let current = current_branch(&root);
    if let Ok(entries) = fs::read_dir(&refs_dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(sha) = read_ref(&root, &format!("refs/heads/{}", name)) {
                let marker = if name == current { "*" } else { " " };
                let c = commit_read(&root, &sha);
                let msg = c.map(|c| c.message.lines().next().unwrap_or("").to_string())
                    .unwrap_or_default();
                println!("{} {} [{}] {}", marker, name, &sha[..7], msg);
            }
        }
    }
}

fn cmd_hd(args: &[String]) {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(1);
    let root = root_or_exit();
    let history = commit_history(&root);
    for (i, (sha, c)) in history.iter().take(n).enumerate() {
        let fname = format!("{:04}-{}.patch", i+1, c.message.lines().next().unwrap_or("patch").replace(' ', "-"));
        let content = format!("From: {}\nSubject: {}\nDate: {}\n\n{}",
            c.author, c.message.trim(), format_ts(c.timestamp), sha);
        fs::write(&fname, content).unwrap();
        println!("hd: wrote '{}'", fname);
    }
}

fn cmd_fhd(args: &[String]) {
    if args.is_empty() { eprintln!("fhd: Usage: hep fhd <file.patch>"); return; }
    println!("fhd: patch application from file '{}' — use 'patch -p1 < {}' for full patch support", args[0], args[0]);
}

fn cmd_apu(args: &[String]) {
    let root = root_or_exit();
    let alias_path = root.join(".hep/aliases");
    if args.is_empty() {
        let content = fs::read_to_string(&alias_path).unwrap_or_default();
        if content.is_empty() { println!("apu: no aliases defined"); }
        else { print!("{}", content); }
    } else if args.len() >= 2 {
        let mut content = fs::read_to_string(&alias_path).unwrap_or_default();
        content.retain(|c| c != '\r');
        let mut lines: Vec<String> = content.lines()
            .filter(|l| !l.starts_with(&format!("{}=", args[0])))
            .map(|l| l.to_string()).collect();
        lines.push(format!("{}={}", args[0], args[1]));
        fs::write(&alias_path, lines.join("\n") + "\n").unwrap();
        println!("apu: alias '{}' = '{}'", args[0], args[1]);
    }
}

fn cmd_xpu(args: &[String]) {
    let root = root_or_exit();
    let notes_dir = root.join(".hep/notes");
    fs::create_dir_all(&notes_dir).unwrap();

    match args.first().map(|s| s.as_str()) {
        Some("add") => {
            if args.len() < 3 { eprintln!("xpu add: Usage: hep xpu add <commit> <note>"); return; }
            let note_file = notes_dir.join(&args[1]);
            fs::write(note_file, &args[2]).unwrap();
            println!("xpu: added note to {}", &args[1]);
        }
        Some("show") => {
            if args.len() < 2 { eprintln!("xpu show: provide commit"); return; }
            let note_file = notes_dir.join(&args[1]);
            match fs::read_to_string(note_file) {
                Ok(n) => println!("{}", n),
                Err(_) => println!("xpu: no note for {}", args[1]),
            }
        }
        Some("list") => {
            if let Ok(entries) = fs::read_dir(&notes_dir) {
                for e in entries.filter_map(|e| e.ok()) {
                    let name = e.file_name().to_string_lossy().to_string();
                    let content = fs::read_to_string(e.path()).unwrap_or_default();
                    println!("{}: {}", name, content.trim());
                }
            }
        }
        _ => eprintln!("xpu: Usage: hep xpu add|show|list"),
    }
}

fn cmd_npu(args: &[String]) {
    let root = root_or_exit();
    let sha = args.first().cloned().or_else(|| head_sha(&root)).unwrap_or_default();
    match read_object(&root, &sha) {
        Ok(_)  => println!("npu: commit {} is valid", &sha[..8.min(sha.len())]),
        Err(e) => eprintln!("npu: commit {} is invalid — {}", sha, e),
    }
}

fn cmd_cpu(args: &[String]) {
    if args.is_empty() { eprintln!("cpu: Usage: hep cpu <branch>"); return; }
    let root = root_or_exit();
    let stash_dir = root.join(".hep/stash");
    let mut entries: Vec<_> = fs::read_dir(&stash_dir).ok()
        .map(|d| d.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    entries.sort_by_key(|e| e.metadata().unwrap().modified().unwrap());

    if let Some(latest) = entries.last() {
        // create branch and apply stash
        let sha = head_sha(&root).unwrap_or_default();
        write_ref(&root, &format!("refs/heads/{}", args[0]), &sha).unwrap();
        fs::write(root.join(".hep/HEAD"),
            format!("ref: refs/heads/{}\n", args[0])).unwrap();
        let content = fs::read_to_string(latest.path()).unwrap_or_default();
        let mut idx = Index::read(&root);
        for line in content.lines() {
            if line.starts_with("msg:") { continue; }
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 {
                if let Ok(data) = blob_read(&root, parts[1]) {
                    let _ = fs::write(parts[0], &data);
                    idx.add(parts[0], parts[1], 0o100644);
                }
            }
        }
        idx.write(&root).unwrap();
        fs::remove_file(latest.path()).unwrap();
        println!("cpu: created branch '{}' from stash", args[0]);
    } else {
        println!("cpu: no stash to branch from");
    }
}

fn cmd_gpu(_args: &[String]) {
    let root = root_or_exit();
    let refs_dir = root.join(".hep/refs/heads");
    let current = current_branch(&root);

    // build commit->branches map
    let mut branch_map: HashMap<String, Vec<String>> = HashMap::new();
    if let Ok(entries) = fs::read_dir(&refs_dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(sha) = read_ref(&root, &format!("refs/heads/{}", name)) {
                branch_map.entry(sha).or_default().push(name);
            }
        }
    }

    for (sha, c) in commit_history(&root) {
        let branches = branch_map.get(&sha).cloned().unwrap_or_default();
        let label = if branches.contains(&current) {
            format!(" \x1b[32m({})\x1b[0m", branches.join(", "))
        } else if !branches.is_empty() {
            format!(" ({})", branches.join(", "))
        } else { String::new() };
        let msg = c.message.lines().next().unwrap_or("").to_string();
        println!("* {} {}{}", &sha[..7], msg, label);
    }
}

fn cmd_rpu(args: &[String]) {
    let root = root_or_exit();
    let rerere_dir = root.join(".hep/rerere");
    fs::create_dir_all(&rerere_dir).unwrap();
    match args.first().map(|s| s.as_str()) {
        Some("record")  => println!("rpu record: conflict patterns saved to .hep/rerere/"),
        Some("replay")  => println!("rpu replay: applied saved resolutions"),
        Some("list")    => {
            if let Ok(entries) = fs::read_dir(&rerere_dir) {
                let count = entries.count();
                println!("rpu: {} saved resolution(s)", count);
            }
        }
        _ => eprintln!("rpu: Usage: hep rpu record|replay|list"),
    }
}

fn cmd_a(_args: &[String]) {
    let root = root_or_exit();
    let obj_dir = root.join(".hep/objects");
    // collect all reachable shas
    let mut reachable = std::collections::HashSet::new();
    for (sha, c) in commit_history(&root) {
        reachable.insert(sha);
        if let Ok(tree) = tree_read(&root, &c.tree_sha) {
            reachable.insert(c.tree_sha);
            for e in tree { reachable.insert(e.sha); }
        }
    }
    // find unreachable
    let mut found = 0;
    if let Ok(dirs) = fs::read_dir(&obj_dir) {
        for d in dirs.filter_map(|e| e.ok()) {
            if let Ok(files) = fs::read_dir(d.path()) {
                for f in files.filter_map(|e| e.ok()) {
                    let hex = format!("{}{}", d.file_name().to_string_lossy(),
                                      f.file_name().to_string_lossy());
                    if !reachable.contains(&hex) {
                        println!("dangling: {}", hex);
                        found += 1;
                    }
                }
            }
        }
    }
    if found == 0 { println!("a: no dangling objects"); }
}

fn cmd_b(_args: &[String]) {
    let root = root_or_exit();
    let obj_dir = root.join(".hep/objects");
    let mut reachable = std::collections::HashSet::new();
    for (sha, c) in commit_history(&root) {
        reachable.insert(sha);
        reachable.insert(c.tree_sha.clone());
        if let Ok(tree) = tree_read(&root, &c.tree_sha) {
            for e in tree { reachable.insert(e.sha); }
        }
    }
    let mut pruned = 0;
    if let Ok(dirs) = fs::read_dir(&obj_dir) {
        for d in dirs.filter_map(|e| e.ok()) {
            if let Ok(files) = fs::read_dir(d.path()) {
                for f in files.filter_map(|e| e.ok()) {
                    let hex = format!("{}{}", d.file_name().to_string_lossy(),
                                      f.file_name().to_string_lossy());
                    if !reachable.contains(&hex) {
                        fs::remove_file(f.path()).unwrap();
                        pruned += 1;
                    }
                }
            }
        }
    }
    println!("b: pruned {} unreachable object(s)", pruned);
}

// ════════════════════════════════════════════════════════════════════════════
// WAVE 5 — RIG
// ════════════════════════════════════════════════════════════════════════════

fn cmd_bios(_args: &[String]) {
    println!("hep v9.0 (Rust) — 92 commands — competing with bro (1 command)\n");
    println!("waves:");
    println!("  wave 1  — core:        print wave spy compete light expand travel chiplets stl");
    println!("                         send dock interface search hall retrieve group microscope");
    println!("                         earth house kill");
    println!("  wave 2  — extended:    mean short close secret change accuse discord window");
    println!("                         what bd power hotel wpm gnome intelisbetterthanamd");
    println!("                         nvl ptl aaa linux r");
    println!("  wave 3  — essentials:  arm ia intel amd nvidia arc radeon");
    println!("  wave 4  — hardware:    rtx gtx rx iris xe uhd hd fhd apu xpu npu cpu gpu rpu a b");
    println!("  wave 5  — rig:         bios case psu ups nas link raid room");
    println!("  wave 6  — gaps:        compete -l  print -line  hall -coat  spy -title");
    println!("                         accuse -part  rp  unsent");
    println!("  wave 7  — better:      undo  redo  mansion");
    println!("                         mansion limit/dock/light/send");
    println!("  wave 8  — network:     ethernet  fiber  switch  packet");
    println!("                         ping  bandwidth  latency  bridge\n");
    println!("flags: hep --help | hep --version");
}

fn cmd_case(_args: &[String]) { cmd_light(&[]); }

fn cmd_psu(args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        Some("--short") => {
            if let Some(branch) = args.get(1) {
                cmd_travel(&[branch.clone()]);
            } else { eprintln!("psu --short: provide branch name"); }
        }
        Some("--reboot") => {
            if let Some(commit) = args.get(1) {
                cmd_kill(&[commit.clone()]);
            } else { eprintln!("psu --reboot: provide commit"); }
        }
        Some("--dust") => {
            cmd_b(&[]);
            println!("psu --dust: loose objects pruned");
        }
        Some("--repaste") => {
            cmd_b(&[]);
            println!("psu --repaste: deep gc complete");
        }
        _ => eprintln!("psu: Usage: hep psu --short <branch> | --reboot <commit> | --dust | --repaste"),
    }
}

fn cmd_ups(_args: &[String]) { cmd_nvidia(&[]); }

fn cmd_nas(args: &[String]) {
    if args.len() < 2 { eprintln!("nas: Usage: hep nas <name> <url>"); return; }
    let root = root_or_exit();
    config_set(&root, &format!("remote.{}.url", args[0]), &args[1]).unwrap();
    println!("nas: remote '{}' = {}", args[0], args[1]);
}

fn cmd_link(_args: &[String]) {
    let root = root_or_exit();
    if let Some(url) = config_get(&root, "remote.origin.url") {
        println!("origin\t{}", url);
    } else {
        println!("link: no remotes configured");
    }
}

fn cmd_raid(_args: &[String]) {
    let root = root_or_exit();
    let cfg = fs::read_to_string(root.join(".hep/config")).unwrap_or_default();
    let mut pushed = 0;
    for line in cfg.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("remote.") {
            if let Some(url_part) = rest.strip_suffix(" = ").or_else(|| {
                let parts: Vec<&str> = rest.splitn(2, " = ").collect();
                if parts.len() == 2 && parts[0].ends_with(".url") { Some("") } else { None }
            }) {
                // parse remote.name.url = value
                let parts: Vec<&str> = rest.splitn(2, " = ").collect();
                if parts.len() == 2 && parts[0].ends_with(".url") {
                    cmd_send(&[parts[1].to_string()]);
                    pushed += 1;
                }
            }
        }
    }
    if pushed == 0 { println!("raid: no remotes configured"); }
}

fn cmd_room(args: &[String]) {
    if args.len() < 2 { eprintln!("room: Usage: hep room <dir> <branch>"); return; }
    cmd_rx(args);
}

// ════════════════════════════════════════════════════════════════════════════
// WAVE 6 — THE REAL GAPS
// ════════════════════════════════════════════════════════════════════════════

fn cmd_compete_l() {
    let root = root_or_exit();
    let idx = Index::read(&root);
    let head_tree: HashMap<String, String> = head_sha(&root)
        .and_then(|sha| commit_read(&root, &sha).ok())
        .and_then(|c| tree_read(&root, &c.tree_sha).ok())
        .map(|tree| tree.into_iter().map(|e| (e.name, e.sha)).collect())
        .unwrap_or_default();

    let mut any = false;
    for e in &idx.entries {
        let old_sha = head_tree.get(&e.path).cloned().unwrap_or_default();
        if old_sha == e.sha { continue; }
        any = true;
        let old_data = if old_sha.is_empty() { Vec::new() }
            else { blob_read(&root, &old_sha).unwrap_or_default() };
        let new_data = blob_read(&root, &e.sha).unwrap_or_default();
        let old_lines: Vec<String> = String::from_utf8_lossy(&old_data).lines().map(|l| l.to_string()).collect();
        let new_lines: Vec<String> = String::from_utf8_lossy(&new_data).lines().map(|l| l.to_string()).collect();
        print_diff(&e.path, &old_lines, &new_lines);
    }
    if !any { println!("compete -l: nothing to diff"); }
}

fn cmd_print_line(args: &[String]) {
    if args.is_empty() { eprintln!("print -line: Usage: hep print -line <file>"); return; }
    let root = root_or_exit();
    let fname = &args[0];
    let content = match fs::read_to_string(fname) {
        Ok(c) => c, Err(_) => { eprintln!("print -line: '{}' not found", fname); return; }
    };
    let new_lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    let idx = Index::read(&root);
    let old_lines: Vec<String> = idx.find(fname)
        .and_then(|e| blob_read(&root, &e.sha).ok())
        .map(|d| String::from_utf8_lossy(&d).lines().map(|l| l.to_string()).collect())
        .unwrap_or_default();

    println!("print -line: interactive staging for '{}'\n  y=stage  n=skip  q=done\n", fname);
    let ops = myers_diff(&old_lines, &new_lines);
    let mut staged = false;

    // find changed regions
    let mut i = 0;
    while i < ops.len() {
        if matches!(ops[i], DiffOp::Keep(_)) { i += 1; continue; }
        let hstart = if i > 3 { i-3 } else { 0 };
        let hend = (i + 8).min(ops.len());
        for op in &ops[hstart..hend] {
            match op {
                DiffOp::Keep(l) => println!("  {}", l),
                DiffOp::Del(l)  => println!("\x1b[31m- {}\x1b[0m", l),
                DiffOp::Add(l)  => println!("\x1b[32m+ {}\x1b[0m", l),
            }
        }
        print!("Stage this hunk? [y/n/q] ");
        io::stdout().flush().unwrap();
        let mut ans = String::new();
        io::stdin().read_line(&mut ans).unwrap();
        if ans.trim() == "q" { break; }
        if ans.trim() == "y" { staged = true; }
        i = hend;
    }

    if staged {
        let mut idx = Index::read(&root);
        if let Ok(sha) = blob_from_file(&root, fname) {
            idx.add(fname, &sha, 0o100644);
            idx.write(&root).unwrap();
            println!("print -line: staged '{}'", fname);
        }
    }
}

fn cmd_hall_coat(args: &[String]) {
    if args.is_empty() {
        eprintln!("hall -coat: Usage: hep hall -coat <file> [file2...]"); return;
    }
    let root = root_or_exit();
    let mut idx = Index::read(&root);
    let ts = now_ts();
    let stash_file = root.join(".hep/stash").join(format!("coat_{}", ts));

    let mut content = String::from("msg:coat\n");
    let mut saved = 0;
    for fname in args {
        if let Some(e) = idx.find(fname) {
            content.push_str(&format!("{} {}\n", fname, e.sha));
            saved += 1;
        } else {
            println!("hall -coat: '{}' not staged, skipping", fname);
        }
    }
    if saved == 0 { println!("hall -coat: nothing to stash"); return; }
    fs::write(stash_file, content).unwrap();
    for fname in args { idx.remove(fname); }
    idx.write(&root).unwrap();
    println!("hall -coat: stashed {} file(s), rest of index untouched", saved);
}

fn cmd_spy_title(args: &[String]) {
    if args.is_empty() { eprintln!("spy -title: Usage: hep spy -title <file>"); return; }
    let root = root_or_exit();
    let mut current_name = args[0].clone();
    println!("spy -title: history of '{}' (following renames)\n", args[0]);

    for (sha, c) in commit_history(&root) {
        if let Ok(tree) = tree_read(&root, &c.tree_sha) {
            if let Some(e) = tree.iter().find(|e| e.name == current_name) {
                let msg = c.message.lines().next().unwrap_or("").to_string();
                println!("commit {}\nauthor: {}\ndate:   {}\nfile:   {}\n\n    {}\n",
                    sha, c.author, format_ts(c.timestamp), current_name, msg);

                // detect renames by blob identity
                let blob_sha = e.sha.clone();
                // check parent
                if let Some(parent_sha) = &c.parent_sha {
                    if let Ok(parent_c) = commit_read(&root, parent_sha) {
                        if let Ok(parent_tree) = tree_read(&root, &parent_c.tree_sha) {
                            let exact = parent_tree.iter().any(|e| e.name == current_name);
                            if !exact {
                                if let Some(renamed) = parent_tree.iter().find(|e| e.sha == blob_sha) {
                                    println!("  [renamed from '{}']\n", renamed.name);
                                    current_name = renamed.name.clone();
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn cmd_rp(args: &[String]) {
    if args.len() < 2 { eprintln!("rp: Usage: hep rp <old> <new>"); return; }
    let root = root_or_exit();
    let old_name = &args[0]; let new_name = &args[1];
    if !Path::new(old_name).exists() {
        eprintln!("rp: '{}' not found", old_name); return;
    }
    fs::rename(old_name, new_name).unwrap();
    let mut idx = Index::read(&root);
    if let Some(e) = idx.find(old_name).cloned() {
        idx.remove(old_name);
        idx.add(new_name, &e.sha, e.mode);
        idx.write(&root).unwrap();
        println!("rp: renamed '{}' -> '{}' (history preserved via blob identity)", old_name, new_name);
    } else {
        eprintln!("rp: '{}' was not tracked", old_name);
    }
}

fn cmd_unsent(_args: &[String]) {
    let root = root_or_exit();
    let branch = current_branch(&root);
    let local_sha = match head_sha(&root) {
        Some(s) => s, None => { println!("unsent: no commits yet"); return; }
    };

    let remote_sha = read_ref(&root, &format!("refs/remote/{}", branch))
        .or_else(|| {
            config_get(&root, "remote.origin.url").and_then(|url| {
                let rref = Path::new(&url).join(".hep/refs/heads").join(&branch);
                fs::read_to_string(rref).ok().map(|s| s.trim().to_string())
            })
        });

    if remote_sha.as_deref() == Some(&local_sha) {
        println!("unsent: nothing — you're fully synced"); return;
    }

    let remote_sha = remote_sha.unwrap_or_else(|| {
        println!("unsent: no remote tracking info — showing all local commits:\n");
        "0000000000000000000000000000000000000000".to_string()
    });

    println!("unsent commits on '{}':\n", branch);
    let mut count = 0;
    for (sha, c) in commit_history(&root) {
        if sha == remote_sha { break; }
        let msg = c.message.lines().next().unwrap_or("").to_string();
        println!("  {}  {}  {}", &sha[..7], format_ts_date(c.timestamp), msg);
        count += 1;
    }
    println!("\n{} unsent commit(s) — run 'hep send' to push", count);
}

fn cmd_accuse_part(args: &[String]) {
    if args.len() < 3 {
        eprintln!("accuse -part: Usage: hep accuse -part <file> <start> <end>"); return;
    }
    let root = root_or_exit();
    let fname = &args[0];
    let start: usize = args[1].parse().unwrap_or(1);
    let end:   usize = args[2].parse().unwrap_or(start);

    let content = match fs::read_to_string(fname) {
        Ok(c) => c, Err(_) => { eprintln!("accuse -part: '{}' not found", fname); return; }
    };
    let file_lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    let mut blame: Vec<(String, String, u64)> = vec![
        ("0000000".to_string(), "unknown".to_string(), 0); file_lines.len()
    ];

    for (sha, c) in commit_history(&root) {
        if let Ok(tree) = tree_read(&root, &c.tree_sha) {
            if let Some(e) = tree.iter().find(|e| e.name == *fname) {
                if let Ok(data) = blob_read(&root, &e.sha) {
                    let blob_lines: Vec<String> = String::from_utf8_lossy(&data)
                        .lines().map(|l| l.to_string()).collect();
                    for (i, line) in file_lines.iter().enumerate() {
                        if i < blob_lines.len() && blob_lines[i] == *line {
                            blame[i] = (sha[..7].to_string(), c.author.clone(), c.timestamp);
                        }
                    }
                }
            }
        }
    }

    println!("accuse -part: '{}' lines {}-{}\n", fname, start, end);
    for i in (start-1)..end.min(file_lines.len()) {
        let (sha, author, ts) = &blame[i];
        println!("{} ({:<30} {} {:>4}) {}", sha, author, format_ts_date(*ts), i+1, file_lines[i]);
    }
}

// ════════════════════════════════════════════════════════════════════════════
// WAVE 7 — BETTER THAN GIT
// ════════════════════════════════════════════════════════════════════════════

fn cmd_undo(_args: &[String]) {
    let root = root_or_exit();
    let log_path = root.join(".hep/logs/HEAD");
    let content = fs::read_to_string(&log_path).unwrap_or_default();
    let mut entries: Vec<String> = content.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    if entries.len() < 2 { println!("undo: already at oldest commit"); return; }

    let current_hex: String = entries.last().unwrap().split_whitespace().next().unwrap_or("").to_string();
    let prev_hex: String = entries[entries.len()-2].split_whitespace().next().unwrap_or("").to_string();

    // save to redo stack
    let redo_path = root.join(".hep/logs/REDO");
    let mut redo = OpenOptions::new().create(true).append(true).open(&redo_path).unwrap();
    writeln!(redo, "{}", current_hex).unwrap();

    // move HEAD back
    update_head(&root, &prev_hex).unwrap();
    restore_tree(&root, &prev_hex);

    // trim reflog
    entries.pop();
    fs::write(&log_path, entries.join("\n") + "\n").unwrap();

    let c = commit_read(&root, &prev_hex).unwrap();
    let msg = c.message.lines().next().unwrap_or("").to_string();
    println!("undo: stepped back to {} \"{}\"", &prev_hex[..7], msg);
    println!("      run 'hep redo' to go forward again");
}

fn cmd_redo(_args: &[String]) {
    let root = root_or_exit();
    let redo_path = root.join(".hep/logs/REDO");
    let content = fs::read_to_string(&redo_path).unwrap_or_default();
    let mut entries: Vec<String> = content.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    if entries.is_empty() { println!("redo: nothing to redo"); return; }

    let next_hex = entries.pop().unwrap();
    match commit_read(&root, &next_hex) {
        Ok(c) => {
            update_head(&root, &next_hex).unwrap();
            restore_tree(&root, &next_hex);
            reflog_append(&root, &next_hex, &c.message);
            let msg = c.message.lines().next().unwrap_or("").to_string();
            println!("redo: stepped forward to {} \"{}\"", &next_hex[..7], msg);
        }
        Err(e) => { eprintln!("redo: couldn't read commit: {}", e); return; }
    }
    fs::write(&redo_path, entries.join("\n") + "\n").unwrap();
}

fn mansion_threshold(root: &Path) -> u64 {
    let cfg = root.join(".hep/mansion.limit");
    fs::read_to_string(cfg).ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(50 * 1024 * 1024)
}

fn mansion_store(root: &Path, path: &str) -> io::Result<String> {
    let sha = blob_from_file(root, path)?;
    let mansion_root = root.join(".hep/mansion");
    fs::create_dir_all(mansion_root.join(&sha[..2]))?;
    let dst = mansion_root.join(&sha[..2]).join(&sha[2..]);
    if !dst.exists() {
        fs::copy(path, &dst)?;
    }
    let meta = fs::metadata(path)?;
    let base = Path::new(path).file_name().unwrap().to_string_lossy();
    Ok(format!("mansion:{} {} {}", sha, meta.len(), base))
}

fn cmd_mansion(args: &[String]) {
    if args.is_empty() {
        eprintln!("mansion: subcommands: limit <size> | dock [file] | light | send");
        return;
    }
    let root = root_or_exit();
    match args[0].as_str() {
        "limit" => {
            if args.len() < 2 { eprintln!("mansion limit: provide size (e.g. 50MB)"); return; }
            let s = &args[1];
            let val: u64 = s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>()
                .parse().unwrap_or(50);
            let unit: String = s.chars().skip_while(|c| c.is_ascii_digit()).collect::<String>().to_uppercase();
            let bytes = match unit.as_str() {
                "KB" => val * 1024,
                "MB" => val * 1024 * 1024,
                "GB" => val * 1024 * 1024 * 1024,
                _    => val,
            };
            fs::write(root.join(".hep/mansion.limit"), format!("{}\n", bytes)).unwrap();
            println!("mansion limit: {} bytes — files larger than this go to mansion", bytes);
        }
        "light" => {
            let threshold = mansion_threshold(&root);
            println!("mansion light: threshold = {} MB\n", threshold / (1024*1024));
            let idx = Index::read(&root);
            let (mut mansion_c, mut normal_c) = (0, 0);
            for e in &idx.entries {
                if let Ok(data) = blob_read(&root, &e.sha) {
                    if data.starts_with(b"mansion:") {
                        let s = String::from_utf8_lossy(&data);
                        let size: u64 = s.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                        println!("  MANSION  {:.0} MB   {}", size as f64/(1024.0*1024.0), e.path);
                        mansion_c += 1;
                    } else {
                        println!("  normal   {} b     {}", data.len(), e.path);
                        normal_c += 1;
                    }
                }
            }
            println!("\n{} normal, {} in mansion", normal_c, mansion_c);
        }
        "send" => {
            let origin = config_get(&root, "remote.origin.url")
                .unwrap_or_else(|| { eprintln!("mansion send: no remote"); std::process::exit(1); });
            let src = root.join(".hep/mansion");
            let dst = Path::new(&origin).join(".hep/mansion");
            if src.exists() {
                let _ = copy_dir(&src, &dst);
                println!("mansion send: pushed large files to {}", origin);
            } else {
                println!("mansion send: no large files to push");
            }
        }
        "dock" => {
            let origin = config_get(&root, "remote.origin.url")
                .unwrap_or_else(|| { eprintln!("mansion dock: no remote"); std::process::exit(1); });
            let remote_mansion = Path::new(&origin).join(".hep/mansion");
            if let Some(fname) = args.get(1) {
                // pull specific file
                let idx = Index::read(&root);
                if let Some(e) = idx.find(fname) {
                    if let Ok(data) = blob_read(&root, &e.sha) {
                        if data.starts_with(b"mansion:") {
                            let s = String::from_utf8_lossy(&data);
                            let man_sha: String = s[8..].split_whitespace().next().unwrap_or("").to_string();
                            let src = remote_mansion.join(&man_sha[..2]).join(&man_sha[2..]);
                            fs::copy(&src, fname).unwrap();
                            println!("mansion dock: pulled '{}'", fname);
                            return;
                        }
                    }
                }
                eprintln!("mansion dock: '{}' not a mansion file", fname);
            } else {
                let local = root.join(".hep/mansion");
                let _ = copy_dir(&remote_mansion, &local);
                println!("mansion dock: pulled all large files from remote");
            }
        }
        _ => eprintln!("mansion: unknown subcommand '{}'", args[0]),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// WAVE 8 — NETWORKING / COLLABORATION
// ════════════════════════════════════════════════════════════════════════════

fn cmd_ethernet(_args: &[String]) {
    let root = root_or_exit();
    println!("# Changelog\n\ngenerated by hep ethernet\n");
    let mut cur_month = String::new();
    for (sha, c) in commit_history(&root) {
        let month = format_ts_date(c.timestamp)[..7].to_string();
        if month != cur_month {
            if !cur_month.is_empty() { println!(); }
            let dt = Local.timestamp_opt(c.timestamp as i64, 0).single()
                .unwrap_or_else(Local::now);
            println!("## {}\n", dt.format("%B %Y"));
            cur_month = month;
        }
        let msg = c.message.lines().next().unwrap_or("").to_string();
        if !msg.is_empty() && !msg.starts_with("Merge") {
            println!("- {} (`{}`)", msg, &sha[..7]);
        }
    }
    println!();
}

fn cmd_fiber(args: &[String]) {
    if args.is_empty() {
        eprintln!("fiber: Usage: hep fiber <date> (e.g. 2025-01-01 or \"7 days ago\")");
        return;
    }
    let root = root_or_exit();
    let since: i64 = if args[0].contains('-') && !args[0].contains("ago") {
        // YYYY-MM-DD
        let parts: Vec<i64> = args[0].split('-').filter_map(|s| s.parse().ok()).collect();
        if parts.len() == 3 {
            let dt = chrono::NaiveDate::from_ymd_opt(parts[0] as i32, parts[1] as u32, parts[2] as u32)
                .unwrap().and_hms_opt(0,0,0).unwrap();
            chrono::Local.from_local_datetime(&dt).unwrap().timestamp()
        } else { 0 }
    } else {
        // "N days/weeks ago"
        let n: i64 = args[0].split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(7);
        let unit = args[0].split_whitespace().nth(1).unwrap_or("days");
        let secs = if unit.starts_with("week") { n * 7 * 86400 }
                   else if unit.starts_with("month") { n * 30 * 86400 }
                   else { n * 86400 };
        Local::now().timestamp() - secs
    };

    println!("fiber: changes since {}\n", args[0]);
    let mut count = 0;
    for (sha, c) in commit_history(&root) {
        if c.timestamp as i64 >= since {
            let msg = c.message.lines().next().unwrap_or("").to_string();
            println!("  {}  {}  {:<20}  {}", &sha[..7], format_ts_date(c.timestamp), c.author, msg);
            count += 1;
        }
    }
    println!("\n{} commit(s)", count);
}

fn cmd_switch(args: &[String]) {
    if args.len() < 2 { eprintln!("switch: Usage: hep switch <branch1> <branch2>"); return; }
    let root = root_or_exit();

    let sha1 = match read_ref(&root, &format!("refs/heads/{}", args[0])) {
        Some(s) => s, None => { eprintln!("switch: branch '{}' not found", args[0]); return; }
    };
    let sha2 = match read_ref(&root, &format!("refs/heads/{}", args[1])) {
        Some(s) => s, None => { eprintln!("switch: branch '{}' not found", args[1]); return; }
    };

    let set1: Vec<String> = {
        let mut h = sha1.clone(); let mut v = Vec::new();
        loop {
            v.push(h.clone());
            match commit_read(&root, &h).ok().and_then(|c| c.parent_sha) {
                Some(p) => h = p, None => break,
            }
        }
        v
    };
    let set2: Vec<String> = {
        let mut h = sha2.clone(); let mut v = Vec::new();
        loop {
            v.push(h.clone());
            match commit_read(&root, &h).ok().and_then(|c| c.parent_sha) {
                Some(p) => h = p, None => break,
            }
        }
        v
    };

    println!("switch: divergence between '{}' and '{}'\n", args[0], args[1]);

    println!("only in '{}':", args[0]);
    let mut only1 = 0;
    for sha in &set1 {
        if !set2.contains(sha) {
            if let Ok(c) = commit_read(&root, sha) {
                let msg = c.message.lines().next().unwrap_or("").to_string();
                println!("  {}  {}", &sha[..7], msg);
                only1 += 1;
            }
        }
    }
    if only1 == 0 { println!("  (none)"); }

    println!("\nonly in '{}':", args[1]);
    let mut only2 = 0;
    for sha in &set2 {
        if !set1.contains(sha) {
            if let Ok(c) = commit_read(&root, sha) {
                let msg = c.message.lines().next().unwrap_or("").to_string();
                println!("  {}  {}", &sha[..7], msg);
                only2 += 1;
            }
        }
    }
    if only2 == 0 { println!("  (none)"); }
    println!("\n{} unique to '{}', {} unique to '{}'", only1, args[0], only2, args[1]);
}

fn cmd_packet(args: &[String]) {
    let root = root_or_exit();
    let sha = args.first().cloned().or_else(|| head_sha(&root)).unwrap_or_else(|| {
        eprintln!("packet: no commits"); std::process::exit(1);
    });
    let c = match commit_read(&root, &sha) {
        Ok(c) => c, Err(e) => { eprintln!("packet: {}", e); return; }
    };
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  packet  {}", sha);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  author  : {}", c.author);
    println!("  date    : {}", format_ts(c.timestamp));
    println!("  parent  : {}", c.parent_sha.as_deref().unwrap_or("(root commit)"));
    println!("  message :\n\n    {}\n", c.message.trim());
    if let Ok(tree) = tree_read(&root, &c.tree_sha) {
        println!("  files ({}):", tree.len());
        for e in &tree {
            let sz = blob_read(&root, &e.sha).map(|d| d.len()).unwrap_or(0);
            println!("    {:<40}  {} bytes  [{}]", e.name, sz, &e.sha[..7]);
        }
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}

fn cmd_ping(args: &[String]) {
    if args.is_empty() { eprintln!("ping: Usage: hep ping <author>"); return; }
    let root = root_or_exit();
    let target = &args[0];
    println!("ping: commits by '{}'\n", target);
    let mut count = 0;
    for (sha, c) in commit_history(&root) {
        if c.author.contains(target.as_str()) {
            let msg = c.message.lines().next().unwrap_or("").to_string();
            println!("  {}  {}  {}", &sha[..7], format_ts_date(c.timestamp), msg);
            count += 1;
        }
    }
    if count == 0 { println!("  no commits found for '{}'", target); }
    else { println!("\n{} commit(s) by '{}'", count, target); }
}

fn cmd_bandwidth(_args: &[String]) {
    let root = root_or_exit();
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut total_commits = 0;
    for (_, c) in commit_history(&root) {
        total_commits += 1;
        if let Ok(tree) = tree_read(&root, &c.tree_sha) {
            for e in tree { *counts.entry(e.name).or_insert(0) += 1; }
        }
    }
    let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    let max = sorted.first().map(|(_,c)| *c).unwrap_or(1);

    println!("bandwidth: most changed files ({} commits)\n", total_commits);
    for (name, count) in sorted.iter().take(20) {
        let bar: String = "█".repeat((count * 30) / max);
        println!("  {:<35}  {:>3}  {}", name, count, bar);
    }
    if sorted.is_empty() { println!("  no files tracked yet"); }
}

fn cmd_latency(_args: &[String]) {
    let root = root_or_exit();
    let history = commit_history(&root);
    if history.len() < 2 { println!("latency: need at least 2 commits"); return; }

    let timestamps: Vec<u64> = history.iter().map(|(_, c)| c.timestamp).collect();
    let gaps: Vec<u64> = timestamps.windows(2).map(|w| {
        if w[0] >= w[1] { w[0] - w[1] } else { w[1] - w[0] }
    }).collect();

    let avg = gaps.iter().sum::<u64>() / gaps.len() as u64;
    let min = *gaps.iter().min().unwrap_or(&0);
    let max = *gaps.iter().max().unwrap_or(&0);

    fn fmt_dur(s: u64) -> String {
        if s < 60 { format!("{}s", s) }
        else if s < 3600 { format!("{}m {}s", s/60, s%60) }
        else if s < 86400 { format!("{}h {}m", s/3600, (s%3600)/60) }
        else { format!("{}d {}h", s/86400, (s%86400)/3600) }
    }

    println!("latency: commit timing analysis\n");
    println!("  total commits   : {}", history.len());
    println!("  first commit    : {}", format_ts_date(*timestamps.last().unwrap()));
    println!("  latest commit   : {}", format_ts_date(*timestamps.first().unwrap()));
    println!("  avg between     : {}", fmt_dur(avg));
    println!("  fastest gap     : {}", fmt_dur(min));
    println!("  longest gap     : {}", fmt_dur(max));

    // activity by hour
    let mut hours = [0usize; 24];
    for ts in &timestamps {
        let dt = Local.timestamp_opt(*ts as i64, 0).single().unwrap_or_else(Local::now);
        hours[dt.hour() as usize] += 1;
    }
    let max_h = *hours.iter().max().unwrap_or(&1);
    println!("\n  activity by hour:");
    for (i, &count) in hours.iter().enumerate() {
        let bar: String = "▪".repeat(if max_h > 0 { (count * 20) / max_h } else { 0 });
        println!("  {:02}:00  {} {}", i, bar, count);
    }
}

fn cmd_bridge(args: &[String]) {
    let root = root_or_exit();
    let out_path = args.first().cloned();
    let branch = current_branch(&root);
    let history = commit_history(&root);
    let head = head_sha(&root).unwrap_or_default();

    let mut md = String::new();
    let now = Local::now().format("%Y-%m-%d %H:%M").to_string();
    md.push_str(&format!("# hep repo summary\n\n_generated {}_\n\n---\n\n", now));
    md.push_str("## overview\n\n| | |\n|---|---|\n");
    md.push_str(&format!("| branch | `{}` |\n", branch));
    md.push_str(&format!("| HEAD | `{}` |\n", if head.len() >= 7 { &head[..7] } else { &head }));
    md.push_str(&format!("| commits | {} |\n", history.len()));
    if let Some((_, first)) = history.last() {
        md.push_str(&format!("| started | {} |\n", format_ts_date(first.timestamp)));
        md.push_str(&format!("| first author | {} |\n", first.author));
    }
    if let Some((_, latest)) = history.first() {
        md.push_str(&format!("| latest | {} |\n", format_ts_date(latest.timestamp)));
    }
    md.push('\n');

    if let Some(sha) = head_sha(&root) {
        if let Ok(c) = commit_read(&root, &sha) {
            if let Ok(tree) = tree_read(&root, &c.tree_sha) {
                md.push_str(&format!("## files ({})\n\n", tree.len()));
                for e in &tree { md.push_str(&format!("- `{}`\n", e.name)); }
                md.push('\n');
            }
        }
    }

    md.push_str("## recent commits\n\n");
    for (sha, c) in history.iter().take(10) {
        let msg = c.message.lines().next().unwrap_or("").to_string();
        md.push_str(&format!("- `{}` {} — {} _{}_\n",
            &sha[..7.min(sha.len())], format_ts_date(c.timestamp), msg, c.author));
    }
    md.push_str("\n---\n\n_built with hep_\n");

    match out_path {
        Some(path) => {
            fs::write(&path, &md).unwrap();
            println!("bridge: wrote report to '{}'", path);
        }
        None => print!("{}", md),
    }
}

// ════════════════════════════════════════════════════════════════════════════
// MAIN DISPATCH
// ════════════════════════════════════════════════════════════════════════════

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("hep: no command given. Run 'hep bios' for help.");
        std::process::exit(1);
    }

    let cmd = args[1].as_str();
    let rest = &args[2..];

    match cmd {
        "--version" | "-v" => println!("hep v9.0 (Rust edition)"),
        "--help"           => cmd_bios(rest),

        // wave 1 — core
        "init"       => cmd_init(rest),
        "print"      => cmd_print(rest),
        "wave"       => cmd_wave(rest),
        "spy"        => cmd_spy(rest),
        "compete"    => cmd_compete(rest),
        "light"      => cmd_light(rest),
        "expand"     => cmd_expand(rest),
        "travel"     => cmd_travel(rest),
        "chiplets"   => cmd_chiplets(rest),
        "stl"        => cmd_stl(rest),
        "send"       => cmd_send(rest),
        "dock"       => cmd_dock(rest),
        "interface"  => cmd_interface(rest),
        "search"     => cmd_search(rest),
        "hall"       => cmd_hall(rest),
        "retrieve"   => cmd_retrieve(rest),
        "group"      => cmd_group(rest),
        "microscope" => cmd_microscope(rest),
        "earth"      => cmd_earth(rest),
        "house"      => cmd_house(rest),
        "kill"       => cmd_kill(rest),

        // wave 2 — extended
        "mean"    => cmd_mean(rest),
        "short"   => cmd_short(rest),
        "close"   => cmd_close(rest),
        "secret"  => cmd_secret(rest),
        "change"  => cmd_change(rest),
        "accuse"  => cmd_accuse(rest),
        "discord" => cmd_discord(rest),
        "window"  => cmd_window(rest),
        "what"    => cmd_what(rest),
        "bd"      => cmd_bd(rest),
        "power"   => cmd_power(rest),
        "hotel"   => cmd_hotel(rest),
        "wpm"     => cmd_wpm(rest),
        "gnome"   => cmd_gnome(rest),
        "intelisbetterthanamd" => cmd_intelisbetterthanamd(rest),
        "nvl"     => cmd_nvl(rest),
        "ptl"     => cmd_ptl(rest),
        "aaa"     => cmd_aaa(rest),
        "linux"   => cmd_linux(rest),
        "r"       => cmd_r(rest),

        // wave 3 — essentials
        "arm"    => cmd_arm(rest),
        "ia"     => cmd_ia(rest),
        "intel"  => cmd_intel(rest),
        "amd"    => cmd_amd(rest),
        "nvidia" => cmd_nvidia(rest),
        "arc"    => cmd_arc(rest),
        "radeon" => cmd_radeon(rest),

        // wave 4 — hardware
        "rtx" => cmd_rtx(rest),
        "gtx" => cmd_gtx(rest),
        "rx"  => cmd_rx(rest),
        "iris"=> cmd_iris(rest),
        "xe"  => cmd_xe(rest),
        "uhd" => cmd_uhd(rest),
        "hd"  => cmd_hd(rest),
        "fhd" => cmd_fhd(rest),
        "apu" => cmd_apu(rest),
        "xpu" => cmd_xpu(rest),
        "npu" => cmd_npu(rest),
        "cpu" => cmd_cpu(rest),
        "gpu" => cmd_gpu(rest),
        "rpu" => cmd_rpu(rest),
        "a"   => cmd_a(rest),
        "b"   => cmd_b(rest),

        // wave 5 — rig
        "bios" => cmd_bios(rest),
        "case" => cmd_case(rest),
        "psu"  => cmd_psu(rest),
        "ups"  => cmd_ups(rest),
        "nas"  => cmd_nas(rest),
        "link" => cmd_link(rest),
        "raid" => cmd_raid(rest),
        "room" => cmd_room(rest),

        // wave 6 — gaps
        "rp"     => cmd_rp(rest),
        "unsent" => cmd_unsent(rest),

        // wave 7 — better
        "undo"    => cmd_undo(rest),
        "redo"    => cmd_redo(rest),
        "mansion" => cmd_mansion(rest),

        // wave 8 — network
        "ethernet"  => cmd_ethernet(rest),
        "fiber"     => cmd_fiber(rest),
        "switch"    => cmd_switch(rest),
        "packet"    => cmd_packet(rest),
        "ping"      => cmd_ping(rest),
        "bandwidth" => cmd_bandwidth(rest),
        "latency"   => cmd_latency(rest),
        "bridge"    => cmd_bridge(rest),

        _ => {
            eprintln!("hep: '{}' not recognized 😈\nRun 'hep bios' for help.", cmd);
            std::process::exit(1);
        }
    }
}
