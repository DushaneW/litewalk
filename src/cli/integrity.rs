//! page, map, check, stats, wal, forensic.

use std::fmt::Write;

use litewalk::{hexdump, FreeRegionKind, PageInfo, Section, Usage, Wal, WalStop, SECTIONS};

use super::dump::hex;
use super::json::Json;
use super::{emit, extra_args, open, path_arg, wal_path, Args, CliError, CliResult, Out};

fn usage_json(u: &Usage) -> Json {
    let mut f = vec![(
        "kind".to_owned(),
        Json::str(u.category().to_lowercase().replace(' ', "-")),
    )];
    match u {
        Usage::Table(n) => f.push(("name".into(), Json::str(&**n))),
        Usage::Index { name, table } => {
            f.push(("name".into(), Json::str(&**name)));
            f.push(("table".into(), Json::str(&**table)));
        }
        Usage::Overflow {
            object,
            owner_page,
            rowid,
        } => {
            f.push(("name".into(), Json::str(&**object)));
            f.push(("owner_page".into(), Json::UInt(u64::from(*owner_page))));
            f.push(("rowid".into(), rowid.map_or(Json::Null, Json::Int)));
        }
        _ => {}
    }
    Json::Obj(f)
}

fn usage_label(page: u32, u: &Usage) -> String {
    match u {
        Usage::Schema if page == 1 => "[HEADER + SQLITE_SCHEMA]".into(),
        Usage::Schema => "[SQLITE_SCHEMA]".into(),
        Usage::Table(n) => format!("[TABLE] {n}"),
        Usage::Index { name, .. } => format!("[INDEX] {name}"),
        Usage::Overflow { object, .. } => format!("[OVERFLOW] {object}"),
        other => format!("[{}]", other.category()),
    }
}

fn usage_detail(u: &Usage) -> Option<String> {
    match u {
        Usage::Overflow {
            owner_page, rowid, ..
        } => Some(match rowid {
            Some(r) => format!("rowid {r}, cell on page {owner_page}"),
            None => format!("cell on page {owner_page}"),
        }),
        Usage::Index { table, .. } => Some(format!("on table {table}")),
        _ => None,
    }
}

pub(super) fn map(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let mut db = open(args, &path)?;
    let map = db.map()?;
    emit(
        args,
        out,
        || {
            Json::Arr(
                map.iter()
                    .map(|e| {
                        Json::obj([
                            ("page", Json::UInt(u64::from(e.page))),
                            ("usage", usage_json(&e.usage)),
                            ("page_type", Json::opt_str(e.page_type.map(|t| t.name()))),
                        ])
                    })
                    .collect(),
            )
        },
        |s| {
            let _ = writeln!(s, "DATABASE MAP\n");
            for e in map
                .iter()
                .skip(args.offset)
                .take(args.limit.unwrap_or(usize::MAX))
            {
                let mut line = format!("{:<6} {}", e.page, usage_label(e.page, &e.usage));
                if args.verbose {
                    if let Some(t) = e.page_type {
                        let _ = write!(line, "  {}", t.name());
                    }
                    if let Some(d) = usage_detail(&e.usage) {
                        let _ = write!(line, "  ({d})");
                    }
                }
                let _ = writeln!(s, "{line}");
            }
        },
    )
}

fn page_json(info: &PageInfo, usage: Option<&Usage>, raw: Option<&[u8]>) -> Json {
    let cells = info
        .cells
        .iter()
        .map(|c| {
            Json::obj([
                ("index", Json::UInt(c.index as u64)),
                ("offset", Json::UInt(c.offset as u64)),
                ("size", Json::UInt(c.size as u64)),
                (
                    "left_child",
                    c.left_child
                        .map_or(Json::Null, |p| Json::UInt(u64::from(p))),
                ),
                ("rowid", c.rowid.map_or(Json::Null, Json::Int)),
                (
                    "payload_size",
                    c.payload_size.map_or(Json::Null, |p| Json::UInt(p as u64)),
                ),
                ("local_size", Json::UInt(c.local_size as u64)),
                (
                    "overflow_page",
                    c.overflow_page
                        .map_or(Json::Null, |p| Json::UInt(u64::from(p))),
                ),
                ("error", Json::opt_str(c.error.clone())),
            ])
        })
        .collect();
    let blocks = info
        .freeblocks
        .iter()
        .map(|b| {
            Json::obj([
                ("offset", Json::UInt(b.offset as u64)),
                ("size", Json::UInt(b.size as u64)),
            ])
        })
        .collect();
    let regions = info
        .regions()
        .into_iter()
        .map(|r| {
            Json::obj([
                ("start", Json::UInt(r.start as u64)),
                ("end", Json::UInt(r.end as u64)),
                ("label", Json::str(r.label)),
            ])
        })
        .collect();
    Json::obj([
        ("page", Json::UInt(u64::from(info.number))),
        ("page_size", Json::UInt(info.page_size as u64)),
        ("usable_size", Json::UInt(info.usable as u64)),
        ("type", Json::opt_str(info.page_type.map(|t| t.name()))),
        ("owner", usage.map_or(Json::Null, usage_json)),
        ("cell_count", Json::UInt(info.cell_count as u64)),
        ("free_bytes", Json::UInt(info.free_bytes as u64)),
        ("unallocated_bytes", Json::UInt(info.unallocated as u64)),
        ("fragmented_bytes", Json::UInt(info.fragmented_bytes as u64)),
        ("first_freeblock", Json::UInt(info.first_freeblock as u64)),
        ("content_start", Json::UInt(info.content_start as u64)),
        (
            "rightmost_child",
            info.rightmost
                .map_or(Json::Null, |p| Json::UInt(u64::from(p))),
        ),
        ("freeblocks", Json::Arr(blocks)),
        ("cells", Json::Arr(cells)),
        ("regions", Json::Arr(regions)),
        (
            "problems",
            Json::Arr(info.problems.iter().map(Json::str).collect()),
        ),
        ("hex", raw.map_or(Json::Null, |b| Json::str(hex(b)))),
    ])
}

pub(super) fn page(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    let n: u32 = args
        .positionals
        .get(1)
        .ok_or_else(|| CliError::Usage("page needs a page number".into()))?
        .parse()
        .map_err(|_| CliError::Usage("page number must be a positive integer".into()))?;
    extra_args(args, 2)?;
    let mut db = open(args, &path)?;
    let info = db.page(n)?;
    let raw = db.page_bytes(n)?;
    let usage = db
        .map()
        .ok()
        .and_then(|m| m.into_iter().nth(n as usize - 1))
        .map(|e| e.usage);

    let mut regions = info.regions();
    if info.page_type.is_none() {
        let first = |end, label: &str| litewalk::Region {
            start: 0,
            end,
            label: label.into(),
        };
        regions = match &usage {
            Some(Usage::Overflow { .. }) => vec![
                first(4, "next overflow page"),
                litewalk::Region {
                    start: 4,
                    end: info.page_size,
                    label: "payload bytes".into(),
                },
            ],
            Some(Usage::FreelistTrunk) => {
                let leaves = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
                vec![
                    first(4, "next trunk page"),
                    litewalk::Region {
                        start: 4,
                        end: 8,
                        label: "leaf count".into(),
                    },
                    litewalk::Region {
                        start: 8,
                        end: (8 + 4 * leaves).min(info.page_size),
                        label: "leaf page numbers".into(),
                    },
                ]
            }
            _ => regions,
        };
    }

    emit(
        args,
        out,
        || page_json(&info, usage.as_ref(), args.hex.then_some(&raw[..])),
        |s| {
            let _ = writeln!(s, "PAGE {n}\n{}", "─".repeat(28));
            let kind = match (info.page_type, &usage) {
                (Some(t), _) => t.name().to_owned(),
                (None, Some(u)) => format!("{} (not a b-tree page)", u.category().to_lowercase()),
                (None, None) => format!("not a b-tree page (first byte {:#04x})", info.type_byte),
            };
            let mut line = |k: &str, v: String| {
                let _ = writeln!(s, "{k:<15}{v}");
            };
            line("type:", kind);
            line("page size:", info.page_size.to_string());
            if let Some(u) = &usage {
                let detail = usage_detail(u)
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default();
                line("owner:", format!("{}{detail}", usage_label(n, u)));
            }
            if info.page_type.is_some() {
                line("cells:", info.cell_count.to_string());
                line("free space:", info.free_bytes.to_string());
                line("fragmented:", info.fragmented_bytes.to_string());
                line("content start:", info.content_start.to_string());
                if let Some(r) = info.rightmost {
                    line("rightmost:", format!("page {r}"));
                }
                if !info.freeblocks.is_empty() {
                    let blocks: Vec<String> = info
                        .freeblocks
                        .iter()
                        .map(|b| format!("{}+{}", b.offset, b.size))
                        .collect();
                    line("free blocks:", blocks.join(", "));
                }
            } else if let Some(Usage::Overflow { .. }) = &usage {
                let next = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
                line(
                    "next:",
                    if next == 0 {
                        "end of chain".into()
                    } else {
                        format!("page {next}")
                    },
                );
            } else if let Some(Usage::FreelistTrunk) = &usage {
                let next = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
                let leaves = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
                line(
                    "next trunk:",
                    if next == 0 {
                        "none".into()
                    } else {
                        format!("page {next}")
                    },
                );
                line("leaf pages:", leaves.to_string());
            }
            for p in &info.problems {
                line("problem:", p.clone());
            }
            for c in info
                .cells
                .iter()
                .skip(args.offset)
                .take(args.limit.unwrap_or(usize::MAX))
            {
                let _ = writeln!(s, "\nCell {}", c.index);
                let mut cell = |k: &str, v: String| {
                    let _ = writeln!(s, "  {k:<13}{v}");
                };
                cell("offset:", c.offset.to_string());
                if let Some(child) = c.left_child {
                    cell("left child:", format!("page {child}"));
                }
                if let Some(p) = c.payload_size {
                    cell("payload:", p.to_string());
                }
                if let Some(r) = c.rowid {
                    cell(
                        if info.page_type.is_some_and(|t| t.is_leaf()) {
                            "rowid:"
                        } else {
                            "max rowid:"
                        },
                        r.to_string(),
                    );
                }
                match c.overflow_page {
                    Some(p) => cell(
                        "overflow:",
                        format!(
                            "yes, first page {p} ({} of {} bytes local)",
                            c.local_size,
                            c.payload_size.unwrap_or(0)
                        ),
                    ),
                    None if c.payload_size.is_some() => cell("overflow:", "no".into()),
                    None => {}
                }
                if let Some(e) = &c.error {
                    cell("error:", e.clone());
                }
            }
            if args.hex {
                let _ = writeln!(s, "\nREGIONS");
                for r in &regions {
                    let _ = writeln!(
                        s,
                        "  {:#06x}..{:#06x}  {:>5} bytes  {}",
                        r.start,
                        r.end,
                        r.end - r.start,
                        r.label
                    );
                }
                let _ = writeln!(s, "\nHEX");
                let label = |offset: usize| {
                    regions
                        .iter()
                        .find(|r| r.start <= offset && offset < r.end)
                        .map(|r| r.label.clone())
                        .unwrap_or_default()
                };
                s.push_str(&hexdump(&raw, label));
            }
        },
    )
}

fn section_json(report: &litewalk::CheckReport, section: Section) -> Json {
    let issues: Vec<Json> = report
        .issues_in(section)
        .iter()
        .map(|i| {
            Json::obj([
                (
                    "page",
                    i.page.map_or(Json::Null, |p| Json::UInt(u64::from(p))),
                ),
                ("message", Json::str(&i.message)),
            ])
        })
        .collect();
    Json::obj([
        ("name", Json::str(section.name())),
        (
            "ok",
            Json::Bool(issues.is_empty() && !report.skipped.contains(&section)),
        ),
        ("skipped", Json::Bool(report.skipped.contains(&section))),
        ("issues", Json::Arr(issues)),
    ])
}

pub(super) fn check(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let report = if args.with_wal && wal_path(&path).metadata().is_ok_and(|m| m.len() > 0) {
        open(args, &path)?.check()?
    } else {
        litewalk::check_reader(std::fs::File::open(&path)?)?
    };
    let valid = report.is_valid();
    emit(
        args,
        out,
        || {
            Json::obj([
                ("valid", Json::Bool(valid)),
                ("page_count", Json::UInt(u64::from(report.page_count))),
                ("tables", Json::UInt(report.tables as u64)),
                ("indexes", Json::UInt(report.indexes as u64)),
                ("rows", Json::UInt(report.rows)),
                (
                    "sections",
                    Json::Arr(SECTIONS.iter().map(|&s| section_json(&report, s)).collect()),
                ),
                (
                    "issues",
                    Json::Arr(
                        report
                            .issues
                            .iter()
                            .map(|i| {
                                Json::obj([
                                    ("section", Json::str(i.section.name())),
                                    (
                                        "page",
                                        i.page.map_or(Json::Null, |p| Json::UInt(u64::from(p))),
                                    ),
                                    ("message", Json::str(&i.message)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ])
        },
        |s| {
            let _ = writeln!(s, "Checking {}\n", path.display());
            for &sec in &SECTIONS {
                let issues = report.issues_in(sec);
                if report.skipped.contains(&sec) && issues.is_empty() {
                    let _ = writeln!(s, "- {} (skipped)", sec.name());
                } else if issues.is_empty() {
                    let _ = writeln!(s, "✓ {}", sec.name());
                } else {
                    let _ = writeln!(s, "✗ {}", sec.name());
                    for i in issues {
                        match i.page {
                            Some(p) => {
                                let _ = writeln!(s, "  page {p}: {}", i.message);
                            }
                            None => {
                                let _ = writeln!(s, "  {}", i.message);
                            }
                        }
                    }
                }
            }
            if valid {
                let _ = writeln!(s, "\nDatabase is structurally valid.");
            } else {
                let _ = writeln!(
                    s,
                    "\nDatabase has {} structural problem{}.",
                    report.issues.len(),
                    if report.issues.len() == 1 { "" } else { "s" }
                );
            }
        },
    )?;
    if valid {
        Ok(())
    } else {
        Err(CliError::Failed)
    }
}

pub(super) fn stats(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let mut db = open(args, &path)?;
    let (mut rows, mut entries, mut tables_read) = (0u64, 0u64, 0u64);
    let mut problems: Vec<String> = Vec::new();
    let tables = db.tables()?;
    for t in &tables {
        if let Some(why) = &t.unsupported {
            problems.push(format!("table {} skipped: {why}", t.name));
            continue;
        }
        tables_read += 1;
        for row in db.scan(t)? {
            match row {
                Ok(_) => rows += 1,
                Err(e) => {
                    problems.push(format!("table {}: {e}", t.name));
                    break;
                }
            }
        }
    }
    let indexes = db.indexes()?;
    for i in &indexes {
        for e in db.scan_index(i)? {
            match e {
                Ok(_) => entries += 1,
                Err(e) => {
                    problems.push(format!("index {}: {e}", i.name));
                    break;
                }
            }
        }
    }
    let st = db.cache_stats();
    let ratio = st.hit_ratio();
    emit(
        args,
        out,
        || {
            Json::obj([
                ("tables_scanned", Json::UInt(tables_read)),
                ("rows", Json::UInt(rows)),
                ("index_entries", Json::UInt(entries)),
                ("cache_capacity_pages", Json::UInt(st.capacity as u64)),
                ("pages_requested", Json::UInt(st.requests())),
                ("cache_hits", Json::UInt(st.hits)),
                ("cache_misses", Json::UInt(st.misses)),
                ("hit_ratio", ratio.map_or(Json::Null, Json::Float)),
                (
                    "problems",
                    Json::Arr(problems.iter().map(Json::str).collect()),
                ),
            ])
        },
        |s| {
            let _ = writeln!(s, "Tables scanned: {tables_read} ({rows} rows)");
            let _ = writeln!(s, "Index entries:  {entries}");
            let _ = writeln!(s, "Cache capacity: {} pages\n", st.capacity);
            let _ = writeln!(s, "Pages read:     {}", st.requests());
            let _ = writeln!(s, "Cache hits:     {}", st.hits);
            let _ = writeln!(s, "Cache misses:   {}", st.misses);
            let _ = writeln!(
                s,
                "Hit ratio:      {}",
                ratio.map_or("n/a".into(), |r| format!("{:.2}%", r * 100.0))
            );
            for p in &problems {
                let _ = writeln!(s, "problem: {p}");
            }
        },
    )
}

fn stop_text(stop: &WalStop) -> &'static str {
    match stop {
        WalStop::EndOfFile => "end of file",
        WalStop::TruncatedFrame => "file ends inside a frame",
        WalStop::SaltMismatch => "frame salt differs from the header (older log generation)",
        WalStop::ChecksumMismatch => "frame checksum mismatch",
        WalStop::InvalidPageNumber => "frame has page number 0",
    }
}

pub(super) fn wal(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let wal_file = wal_path(&path);
    let wal = Wal::open(&wal_file)?;
    let h = &wal.header;
    emit(
        args,
        out,
        || {
            let frames = if args.verbose {
                Json::Arr(
                    wal.frames
                        .iter()
                        .map(|f| {
                            Json::obj([
                                ("index", Json::UInt(f.index as u64)),
                                ("page", Json::UInt(u64::from(f.page))),
                                (
                                    "commit_size",
                                    f.commit_size
                                        .map_or(Json::Null, |c| Json::UInt(u64::from(c))),
                                ),
                            ])
                        })
                        .collect(),
                )
            } else {
                Json::Null
            };
            Json::obj([
                ("file", Json::str(wal_file.display().to_string())),
                ("page_size", Json::UInt(u64::from(h.page_size))),
                ("version", Json::UInt(u64::from(h.version))),
                (
                    "checkpoint_sequence",
                    Json::UInt(u64::from(h.checkpoint_sequence)),
                ),
                ("salt1", Json::UInt(u64::from(h.salt1))),
                ("salt2", Json::UInt(u64::from(h.salt2))),
                ("big_endian_checksums", Json::Bool(h.big_endian_checksums)),
                ("valid_frames", Json::UInt(wal.frames.len() as u64)),
                ("committed_frames", Json::UInt(wal.committed_frames as u64)),
                ("committed_pages", Json::UInt(wal.committed_pages() as u64)),
                (
                    "db_size_after_commit",
                    wal.db_size_after_commit
                        .map_or(Json::Null, |c| Json::UInt(u64::from(c))),
                ),
                ("stopped_because", Json::str(stop_text(&wal.stop))),
                ("trailing_bytes", Json::UInt(wal.trailing_bytes as u64)),
                ("frames", frames),
            ])
        },
        |s| {
            let _ = writeln!(s, "WAL {}\n", wal_file.display());
            let _ = writeln!(s, "frames:       {}", wal.frames.len());
            let _ = writeln!(s, "committed:    {}", wal.committed_frames);
            let _ = writeln!(s, "page size:    {}", h.page_size);
            let _ = writeln!(s, "checkpoint:   sequence {}", h.checkpoint_sequence);
            let _ = writeln!(s, "salts:        {:#010x} {:#010x}", h.salt1, h.salt2);
            let _ = writeln!(
                s,
                "checksums:    {}-endian",
                if h.big_endian_checksums {
                    "big"
                } else {
                    "little"
                }
            );
            if let Some(size) = wal.db_size_after_commit {
                let _ = writeln!(s, "db size:      {size} pages after the last commit");
            }
            let _ = writeln!(
                s,
                "distinct pages in committed frames: {}",
                wal.committed_pages()
            );
            if wal.stop != WalStop::EndOfFile {
                let _ = writeln!(
                    s,
                    "log ended:    {} ({} trailing bytes ignored)",
                    stop_text(&wal.stop),
                    wal.trailing_bytes
                );
            }
            if args.verbose {
                let _ = writeln!(s, "\nframe  page  commit");
                for f in &wal.frames {
                    let _ = writeln!(
                        s,
                        "{:<6} {:<5} {}",
                        f.index,
                        f.page,
                        f.commit_size
                            .map_or(String::new(), |c| format!("commit, db size {c}"))
                    );
                }
            }
        },
    )
}

pub(super) fn forensic(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let mut db = open(args, &path)?;
    let r = db.forensic()?;
    let regions = if args.free_space {
        db.free_regions()?
    } else {
        Vec::new()
    };
    let wal_len = wal_path(&path).metadata().map(|m| m.len()).ok();
    emit(
        args,
        out,
        || {
            let objs = |v: &[litewalk::ObjectSize]| {
                Json::Arr(
                    v.iter()
                        .map(|o| {
                            Json::obj([
                                ("name", Json::str(&o.name)),
                                ("pages", Json::UInt(u64::from(o.pages))),
                                ("cells", Json::UInt(o.cells)),
                            ])
                        })
                        .collect(),
                )
            };
            Json::obj([
                ("file_size", Json::UInt(r.file_size)),
                ("page_count", Json::UInt(u64::from(r.page_count))),
                ("page_size", Json::UInt(r.page_size as u64)),
                ("tables", Json::UInt(r.tables as u64)),
                ("indexes", Json::UInt(r.indexes as u64)),
                ("overflow_pages", Json::UInt(r.overflow_pages)),
                ("freelist_pages", Json::UInt(r.freelist_pages)),
                (
                    "largest_object",
                    r.largest_object.as_ref().map_or(Json::Null, |o| {
                        Json::obj([
                            ("name", Json::str(&o.name)),
                            ("pages", Json::UInt(u64::from(o.pages))),
                            ("cells", Json::UInt(o.cells)),
                        ])
                    }),
                ),
                (
                    "largest_payload",
                    r.largest_payload.as_ref().map_or(Json::Null, |(n, o, id)| {
                        Json::obj([
                            ("bytes", Json::UInt(*n as u64)),
                            ("object", Json::str(o)),
                            ("rowid", id.map_or(Json::Null, Json::Int)),
                        ])
                    }),
                ),
                ("wal_file_bytes", wal_len.map_or(Json::Null, Json::UInt)),
                (
                    "free_space",
                    Json::obj([
                        (
                            "unallocated_bytes",
                            Json::UInt(r.free_space.unallocated_bytes),
                        ),
                        ("freeblock_bytes", Json::UInt(r.free_space.freeblock_bytes)),
                        ("freeblock_count", Json::UInt(r.free_space.freeblock_count)),
                        (
                            "fragmented_bytes",
                            Json::UInt(r.free_space.fragmented_bytes),
                        ),
                    ]),
                ),
                ("objects", objs(&r.objects)),
                (
                    "anomalies",
                    Json::Arr(
                        r.anomalies
                            .iter()
                            .map(|i| {
                                Json::obj([
                                    ("section", Json::str(i.section.name())),
                                    (
                                        "page",
                                        i.page.map_or(Json::Null, |p| Json::UInt(u64::from(p))),
                                    ),
                                    ("message", Json::str(&i.message)),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "free_regions",
                    if args.free_space {
                        Json::Arr(
                            regions
                                .iter()
                                .map(|g| {
                                    Json::obj([
                                        ("page", Json::UInt(u64::from(g.page))),
                                        (
                                            "kind",
                                            Json::str(if g.kind == FreeRegionKind::FreeBlock {
                                                "free-block"
                                            } else {
                                                "unallocated"
                                            }),
                                        ),
                                        ("offset", Json::UInt(g.offset as u64)),
                                        ("length", Json::UInt(g.len as u64)),
                                    ])
                                })
                                .collect(),
                        )
                    } else {
                        Json::Null
                    },
                ),
            ])
        },
        |s| {
            let _ = writeln!(
                s,
                "FORENSIC REPORT (read-only structural analysis; nothing is recovered)\n"
            );
            let mut line = |k: &str, v: String| {
                let _ = writeln!(s, "{k:<18}{v}");
            };
            line("file size:", format!("{} bytes", r.file_size));
            line("page size:", r.page_size.to_string());
            line("pages:", r.page_count.to_string());
            line("tables:", r.tables.to_string());
            line("indexes:", r.indexes.to_string());
            line("overflow pages:", r.overflow_pages.to_string());
            line("freelist pages:", r.freelist_pages.to_string());
            if let Some(o) = &r.largest_object {
                line(
                    "largest object:",
                    format!("{} ({} pages, {} cells)", o.name, o.pages, o.cells),
                );
            }
            if let Some((n, o, id)) = &r.largest_payload {
                line(
                    "largest payload:",
                    format!(
                        "{n} bytes ({o}{})",
                        id.map_or(String::new(), |i| format!(", rowid {i}"))
                    ),
                );
            }
            line(
                "WAL file:",
                match wal_len {
                    Some(n) if n > 0 && args.with_wal => {
                        format!("present, {n} bytes (committed frames applied)")
                    }
                    Some(n) if n > 0 => format!("present, {n} bytes (not applied)"),
                    Some(_) => "present, empty".into(),
                    None => "absent".into(),
                },
            );
            let f = &r.free_space;
            line("unused space:", format!("{} unallocated, {} in {} free blocks, {} fragmented (bytes, b-tree pages only)", f.unallocated_bytes, f.freeblock_bytes, f.freeblock_count, f.fragmented_bytes));
            if r.anomalies.is_empty() {
                line("anomalies:", "none".into());
            } else {
                line("anomalies:", r.anomalies.len().to_string());
                for i in &r.anomalies {
                    let _ = writeln!(
                        s,
                        "  [{}] {}{}",
                        i.section.name(),
                        i.page.map_or(String::new(), |p| format!("page {p}: ")),
                        i.message
                    );
                }
            }
            if args.verbose {
                let _ = writeln!(s, "\nobjects:");
                for o in &r.objects {
                    let _ = writeln!(
                        s,
                        "  {:<30} {:>6} pages {:>10} cells",
                        o.name, o.pages, o.cells
                    );
                }
            }
            if args.free_space {
                let _ = writeln!(s, "\nEXPERIMENTAL: unused regions inside b-tree pages (largest first). Stale bytes may be present; they are not interpreted.");
                for g in regions.iter().take(args.limit.unwrap_or(20)) {
                    let kind = if g.kind == FreeRegionKind::FreeBlock {
                        "free block "
                    } else {
                        "unallocated"
                    };
                    let _ = writeln!(
                        s,
                        "  page {:<6} {kind} offset {:<6} {:>6} bytes",
                        g.page, g.offset, g.len
                    );
                }
                if regions.len() > args.limit.unwrap_or(20) {
                    let _ = writeln!(
                        s,
                        "  ... {} more (use --limit)",
                        regions.len() - args.limit.unwrap_or(20)
                    );
                }
            }
        },
    )
}
